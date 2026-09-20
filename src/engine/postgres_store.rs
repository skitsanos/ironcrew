#![cfg(feature = "postgres")]

use std::time::Duration;

use async_trait::async_trait;
use sqlx::{PgPool, Row};
use tokio::sync::Semaphore;

use crate::utils::error::{IronCrewError, Result};

mod codecs;
mod conversations;
mod human_input;
mod idempotency;
mod run_events;
mod run_history;

use codecs::{decode_stored_json, parse_timestamp};
use human_input::validate_human_input_route;

/// Upper bound on the per-retry backoff delay during store init.
const CONNECT_BACKOFF_CAP_MS: u64 = 30_000;
const MAX_DB_POOL_SIZE: u32 = 128;
const MAX_CONNECT_RETRIES: u32 = 100;
const MAX_CONNECT_TIMEOUT_SECS: u64 = 120;
const MAX_TABLE_PREFIX_BYTES: usize = 37;
const MAX_DURABLE_HUMAN_INPUT_ROWS: usize = 256;
const HUMAN_INPUT_AEAD_OVERHEAD_BYTES: usize = 28;
const DEFAULT_HUMAN_INPUT_READ_CONCURRENCY: usize = 8;
const MAX_HUMAN_INPUT_READ_CONCURRENCY: usize = 64;
const HUMAN_INPUT_READ_CONCURRENCY_ENV: &str = "IRONCREW_HITL_PG_MAX_CONCURRENT_READS";
const MIN_ACCOUNTED_RUN_EVENT_BYTES: i64 = 1024;
const MAX_EVICTED_RUN_EVENTS_PER_APPEND: u64 = 65_536;
const RUN_RECONCILIATION_BATCH_SIZE: i64 = 64;
const RUN_EVENT_SCHEMA_VERSION: i32 = 1;
fn validate_table_prefix(table_prefix: &str) -> Result<()> {
    if table_prefix.len() > MAX_TABLE_PREFIX_BYTES
        || !table_prefix
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
    {
        return Err(IronCrewError::Validation(format!(
            "Invalid IRONCREW_PG_TABLE_PREFIX '{}': maximum {MAX_TABLE_PREFIX_BYTES} lowercase ASCII alphanumeric/underscore bytes",
            table_prefix
        )));
    }
    Ok(())
}

fn parse_env<T>(name: &str, default: T) -> Result<T>
where
    T: std::str::FromStr,
{
    match std::env::var(name) {
        Ok(value) => value
            .parse::<T>()
            .map_err(|_| IronCrewError::Validation(format!("{name} has an invalid numeric value"))),
        Err(std::env::VarError::NotPresent) => Ok(default),
        Err(std::env::VarError::NotUnicode(_)) => Err(IronCrewError::Validation(format!(
            "{name} must be valid UTF-8"
        ))),
    }
}

use super::human_input::{
    DurableHumanInputRegistration, HumanInputAnswerOutcome, HumanInputKeyring,
    HumanInputListOutcome, HumanInputReadOutcome, HumanInputRegistrationOutcome,
};
use super::idempotency::{
    ConversationIdempotencyCommit, IdempotencyClaim, IdempotencyClaimOutcome,
    IdempotencyCompletion, IdempotencyCompletionOutcome, IdempotencyLimits, IdempotencyLookup,
    IdempotencyState, IdempotencyUsage, PrincipalId, RUN_OPERATION, RunCancellationRequest,
    RunFenceHeartbeat,
};
use super::input_bridge::{max_pending, max_pending_bytes};
use super::run_events::{
    EventJournalScope, HARD_MAX_EVENT_BYTES, RunEventAppendBatch, RunEventAppendOutcome,
    RunEventJournalConfig, RunEventPage,
};
use super::run_history::{
    ListRunsFilter, RunCompletion, RunIntent, RunRecord, RunStatus, RunSummary, RunTransition,
};
use super::sessions::{ConversationRecord, ConversationSummary, DialogStateRecord};
use super::store::{ConversationCoordinationScope, RunLeaseConfig, StateStore};
use super::store_sql::{self, Dialect, SqlParam, WhereClause};

/// Fold the shared builder's ordered params onto a sqlx query via `.bind`.
/// The `success` filter is bound as a native `bool`, matching the `BOOLEAN`
/// `success` column on the `audit_events` table.
fn bind_params<'q>(
    mut query: sqlx::query::Query<'q, sqlx::Postgres, sqlx::postgres::PgArguments>,
    params: &'q [SqlParam],
) -> sqlx::query::Query<'q, sqlx::Postgres, sqlx::postgres::PgArguments> {
    for p in params {
        query = match p {
            SqlParam::Text(s) => query.bind(s),
            SqlParam::Bool(b) => query.bind(b),
        };
    }
    query
}

pub struct PostgresStore {
    pool: PgPool,
    table_name: String,
    conversations_table: String,
    dialogs_table: String,
    audit_events_table: String,
    idempotency_table: String,
    idempotency_accounting_table: String,
    human_inputs_table: String,
    run_events_table: String,
    run_event_state_table: String,
    run_event_usage_table: String,
    human_input_keyring: Option<HumanInputKeyring>,
    human_input_max_pending_rows: usize,
    human_input_max_pending_ciphertext_bytes: usize,
    human_input_read_slots: Semaphore,
    run_event_journal_config: RunEventJournalConfig,
    lease: RunLeaseConfig,
}

impl PostgresStore {
    /// Create a new PostgreSQL store.
    /// `table_prefix` allows sharing a database across projects:
    ///   prefix = "myapp_" → table = "myapp_runs"
    ///   prefix = "" → table = "runs" (default)
    pub async fn new(database_url: &str, table_prefix: &str) -> Result<Self> {
        Self::new_with_lease_config(database_url, table_prefix, RunLeaseConfig::from_env()?).await
    }

    pub async fn new_with_lease_config(
        database_url: &str,
        table_prefix: &str,
        lease: RunLeaseConfig,
    ) -> Result<Self> {
        let keyring = HumanInputKeyring::from_env()?;
        Self::new_with_lease_config_and_human_input_keyring(
            database_url,
            table_prefix,
            lease,
            keyring,
        )
        .await
    }

    /// Construct a store with an explicit durable human-input keyring.
    ///
    /// Production callers normally use [`Self::new_with_lease_config`], which
    /// loads the keyring once from the environment. This constructor keeps
    /// live-database tests deterministic and avoids process-global env races.
    pub async fn new_with_lease_config_and_human_input_keyring(
        database_url: &str,
        table_prefix: &str,
        lease: RunLeaseConfig,
        human_input_keyring: Option<HumanInputKeyring>,
    ) -> Result<Self> {
        let run_event_journal_config = RunEventJournalConfig::from_env()?;
        Self::new_with_runtime_config(
            database_url,
            table_prefix,
            lease,
            human_input_keyring,
            run_event_journal_config,
        )
        .await
    }

    /// Construct a store with deterministic process-wide runtime features.
    /// Production constructors load these immutable values once from env;
    /// live PostgreSQL tests use this entrypoint to avoid environment races.
    pub async fn new_with_runtime_config(
        database_url: &str,
        table_prefix: &str,
        lease: RunLeaseConfig,
        human_input_keyring: Option<HumanInputKeyring>,
        run_event_journal_config: RunEventJournalConfig,
    ) -> Result<Self> {
        // Validate table prefix to prevent SQL injection via env var
        validate_table_prefix(table_prefix)?;
        run_event_journal_config.validate()?;
        let human_input_max_pending_rows = max_pending().min(MAX_DURABLE_HUMAN_INPUT_ROWS);
        let human_input_max_pending_ciphertext_bytes = max_pending_bytes()
            .checked_add(
                human_input_max_pending_rows.saturating_mul(HUMAN_INPUT_AEAD_OVERHEAD_BYTES),
            )
            .ok_or_else(|| {
                IronCrewError::Validation("PostgreSQL human-input ciphertext limit overflow".into())
            })?;
        let human_input_read_concurrency: usize = parse_env(
            HUMAN_INPUT_READ_CONCURRENCY_ENV,
            DEFAULT_HUMAN_INPUT_READ_CONCURRENCY,
        )?;
        if !(1..=MAX_HUMAN_INPUT_READ_CONCURRENCY).contains(&human_input_read_concurrency) {
            return Err(IronCrewError::Validation(format!(
                "{HUMAN_INPUT_READ_CONCURRENCY_ENV} must be between 1 and {MAX_HUMAN_INPUT_READ_CONCURRENCY}"
            )));
        }

        let max_conn: u32 = parse_env("IRONCREW_DB_POOL_SIZE", 10)?;
        if max_conn == 0 || max_conn > MAX_DB_POOL_SIZE {
            return Err(IronCrewError::Validation(format!(
                "IRONCREW_DB_POOL_SIZE must be between 1 and {MAX_DB_POOL_SIZE}"
            )));
        }

        // Retries *after* the initial attempt. With backoff this rides out a
        // transient database outage (e.g. a platform restart) so a brief blip
        // doesn't crash the process and burn a container restart per attempt.
        let retries: u32 = parse_env("IRONCREW_DB_CONNECT_RETRIES", 10)?;
        if retries > MAX_CONNECT_RETRIES {
            return Err(IronCrewError::Validation(format!(
                "IRONCREW_DB_CONNECT_RETRIES must be at most {MAX_CONNECT_RETRIES}"
            )));
        }
        let backoff_base_ms: u64 = parse_env("IRONCREW_DB_CONNECT_BACKOFF_MS", 1_000)?;
        if backoff_base_ms == 0 || backoff_base_ms > CONNECT_BACKOFF_CAP_MS {
            return Err(IronCrewError::Validation(format!(
                "IRONCREW_DB_CONNECT_BACKOFF_MS must be between 1 and {CONNECT_BACKOFF_CAP_MS}"
            )));
        }
        let connect_timeout_secs: u64 = parse_env("IRONCREW_DB_CONNECT_TIMEOUT_SECS", 30)?;
        if connect_timeout_secs == 0 || connect_timeout_secs > MAX_CONNECT_TIMEOUT_SECS {
            return Err(IronCrewError::Validation(format!(
                "IRONCREW_DB_CONNECT_TIMEOUT_SECS must be between 1 and {MAX_CONNECT_TIMEOUT_SECS}"
            )));
        }

        let pool = crate::engine::pg_runtime::connect_pool(
            database_url,
            &crate::engine::pg_runtime::PgConnectSettings {
                max_connections: max_conn,
                acquire_timeout: Duration::from_secs(connect_timeout_secs),
                retries,
                backoff_base_ms,
                backoff_cap_ms: CONNECT_BACKOFF_CAP_MS,
            },
            "state store",
        )
        .await?;

        crate::engine::pg_runtime::ensure_supported_postgres_version(&pool).await?;

        let table_name = format!("{}runs", table_prefix);
        let conversations_table = format!("{}conversations", table_prefix);
        let dialogs_table = format!("{}dialogs", table_prefix);
        let audit_events_table = format!("{}audit_events", table_prefix);
        let idempotency_table = format!("{}idempotency", table_prefix);
        let idempotency_accounting_table = format!("{}idempotency_accounting", table_prefix);
        let human_inputs_table = format!("{}human_inputs", table_prefix);
        let run_events_table = format!("{}run_events", table_prefix);
        let run_event_state_table = format!("{}run_event_state", table_prefix);
        let run_event_usage_table = format!("{}run_event_usage", table_prefix);

        let store = Self {
            pool,
            table_name: table_name.clone(),
            conversations_table,
            dialogs_table,
            audit_events_table,
            idempotency_table,
            idempotency_accounting_table,
            human_inputs_table,
            run_events_table,
            run_event_state_table,
            run_event_usage_table,
            human_input_keyring,
            human_input_max_pending_rows,
            human_input_max_pending_ciphertext_bytes,
            human_input_read_slots: Semaphore::new(human_input_read_concurrency),
            run_event_journal_config,
            lease,
        };
        store.bootstrap().await?;
        store
            .verify_human_input_key_coverage(Duration::from_secs(connect_timeout_secs))
            .await?;

        tracing::info!("PostgreSQL store ready (table: {})", table_name);
        Ok(store)
    }

    async fn lock_advisory(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        domain: &str,
        identity: &str,
        shared: bool,
    ) -> Result<()> {
        let lock_name = format!(
            "ironcrew:{}:{domain}:{}:{identity}",
            self.idempotency_table,
            identity.len()
        );
        let sql = if shared {
            "SELECT pg_advisory_xact_lock_shared(hashtextextended($1, 0))"
        } else {
            "SELECT pg_advisory_xact_lock(hashtextextended($1, 0))"
        };
        sqlx::query(sql)
            .bind(lock_name)
            .execute(&mut **tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "Failed to acquire PostgreSQL {domain} advisory lock: {error}"
                ))
            })?;
        Ok(())
    }

    async fn configure_run_lease_transaction(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    ) -> Result<()> {
        // PostgreSQL applies these limits per statement, not to the aggregate
        // transaction. The outer maintenance watchdog may therefore fire
        // first after several individually successful statements. Dropping
        // SQLx's owned transaction schedules a rollback before that pooled
        // connection can be reused; the maintenance regression suite covers
        // both the atomic rollback and subsequent pool recovery.
        let timeout = crate::engine::store::run_maintenance_database_timeout(self.lease.ttl());
        let timeout_value = format!("{}ms", timeout.as_millis());
        sqlx::query(
            "SELECT set_config('lock_timeout', $1, true), \
                    set_config('statement_timeout', $1, true)",
        )
        .bind(timeout_value)
        .execute(&mut **tx)
        .await
        .map_err(|error| {
            IronCrewError::Validation(format!(
                "Failed to configure PostgreSQL run-lease transaction timeouts: {error}"
            ))
        })?;
        Ok(())
    }

    async fn lock_resource(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        operation: &str,
        scope: &str,
        resource_id: &str,
    ) -> Result<()> {
        let identity = format!(
            "{}:{operation}:{}:{scope}:{}:{resource_id}",
            operation.len(),
            scope.len(),
            resource_id.len()
        );
        self.lock_advisory(tx, "resource", &identity, false).await
    }

    async fn lock_run_fence(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        shared: bool,
    ) -> Result<()> {
        self.lock_advisory(tx, "run-fence", "global", shared).await
    }

    async fn materialize_abandoned_claim_batch(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        database_now: &str,
        limit: i64,
    ) -> Result<Vec<String>> {
        if limit <= 0 {
            return Ok(Vec::new());
        }
        let sql = format!(
            "WITH candidates AS (\
                 SELECT idem.resource_id, idem.scope, idem.created_at, \
                        idem.owner_instance_id \
                 FROM {idempotency} AS idem \
                 WHERE idem.operation = $2 AND idem.state = 'claimed' \
                   AND idem.lease_expires_at::timestamptz <= $3::timestamptz \
                   AND NOT EXISTS (\
                       SELECT 1 FROM {runs} AS run \
                       WHERE run.run_id = idem.resource_id\
                   ) \
                 ORDER BY idem.lease_expires_at, idem.key_hash \
                 LIMIT $4 FOR UPDATE OF idem SKIP LOCKED\
             ) \
             INSERT INTO {runs} (\
                 run_id, flow_name, flow, status, started_at, finished_at, duration_ms, \
                 task_results, agent_count, task_count, total_tokens, cached_tokens, tags, \
                 owner_instance_id, lease_expires_at\
             ) \
             SELECT resource_id, scope, scope, 'abandoned', created_at, $1, 0, \
                    '[]'::jsonb, 0, 0, 0, 0, '[]'::jsonb, owner_instance_id, '' \
             FROM candidates \
             ON CONFLICT (run_id) DO NOTHING \
             RETURNING run_id",
            runs = self.table_name,
            idempotency = self.idempotency_table,
        );
        let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(database_now)
            .bind(RUN_OPERATION)
            .bind(database_now)
            .bind(limit)
            .fetch_all(&mut **tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!("PG idempotent run fallback: {error}"))
            })?;
        rows.into_iter()
            .map(|row| {
                row.try_get("run_id").map_err(|error| {
                    IronCrewError::Validation(format!("PostgreSQL fallback run id column: {error}"))
                })
            })
            .collect()
    }

    async fn reconcile_expired_run_batch(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        database_now: &str,
        limit: i64,
    ) -> Result<Vec<String>> {
        if limit <= 0 {
            return Ok(Vec::new());
        }
        let sql = format!(
            "WITH candidates AS (\
                 SELECT run_id FROM {runs} \
                 WHERE status IN ('running', 'waiting_for_input') \
                   AND (lease_expires_at = '' OR \
                        lease_expires_at::timestamptz <= $2::timestamptz) \
                 ORDER BY lease_expires_at, run_id \
                 LIMIT $3 FOR UPDATE SKIP LOCKED\
             ) \
             UPDATE {runs} AS run \
             SET status = 'abandoned', finished_at = $1, lease_expires_at = '' \
             FROM candidates \
             WHERE run.run_id = candidates.run_id \
             RETURNING run.run_id",
            runs = self.table_name,
        );
        let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(database_now)
            .bind(database_now)
            .bind(limit)
            .fetch_all(&mut **tx)
            .await
            .map_err(|error| IronCrewError::Validation(format!("PG reconcile: {error}")))?;
        rows.into_iter()
            .map(|row| {
                row.try_get("run_id").map_err(|error| {
                    IronCrewError::Validation(format!(
                        "PostgreSQL reconciled run id column: {error}"
                    ))
                })
            })
            .collect()
    }

    async fn finalize_reconciled_runs(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        database_now: &str,
        run_ids: &[String],
    ) -> Result<()> {
        if run_ids.is_empty() {
            return Ok(());
        }
        let journal_sql = format!(
            "UPDATE {} SET journal_complete = FALSE, updated_at = clock_timestamp() \
             WHERE run_id = ANY($1::text[])",
            self.run_event_state_table,
        );
        sqlx::query(sqlx::AssertSqlSafe(journal_sql))
            .bind(run_ids)
            .execute(&mut **tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL abandoned run-event journal update failed: {error}"
                ))
            })?;

        let mapping_sql = format!(
            "UPDATE {idempotency} \
             SET state = 'completed', lease_expires_at = '', \
                 updated_at = $2, completed_at = $2, \
                 expires_at = to_char(\
                     ($2::timestamptz + ttl_seconds * interval '1 second') \
                         AT TIME ZONE 'UTC', \
                     'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"'\
                 ) \
             WHERE operation = $1 AND resource_id = ANY($3::text[]) \
               AND state IN ('claimed', 'running', 'indeterminate')",
            idempotency = self.idempotency_table,
        );
        sqlx::query(sqlx::AssertSqlSafe(mapping_sql))
            .bind(RUN_OPERATION)
            .bind(database_now)
            .bind(run_ids)
            .execute(&mut **tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PG reconciled run idempotency transition: {error}"
                ))
            })?;

        let mailbox_sql = format!(
            "DELETE FROM {} WHERE run_id = ANY($1::text[])",
            self.human_inputs_table,
        );
        sqlx::query(sqlx::AssertSqlSafe(mailbox_sql))
            .bind(run_ids)
            .execute(&mut **tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL reconciled run mailbox cleanup failed: {error}"
                ))
            })?;
        Ok(())
    }

    async fn database_clock_with_deadline(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        seconds: u64,
        context: &str,
    ) -> Result<(String, String)> {
        let seconds = i64::try_from(seconds).map_err(|_| {
            IronCrewError::Validation(format!("PostgreSQL {context} duration is out of range"))
        })?;
        sqlx::query_as::<_, (String, String)>(
            "WITH db_clock AS (SELECT clock_timestamp() AS now) \
             SELECT \
                 to_char(now AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"'), \
                 to_char(\
                     (now + $1::bigint * interval '1 second') AT TIME ZONE 'UTC', \
                     'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"'\
                 ) \
             FROM db_clock",
        )
        .bind(seconds)
        .fetch_one(&mut **tx)
        .await
        .map_err(|error| {
            IronCrewError::Validation(format!(
                "Failed to read PostgreSQL clock for {context}: {error}"
            ))
        })
    }

    /// Bootstrap the database: create table, add missing columns, fix types, create indexes.
    async fn bootstrap(&self) -> Result<()> {
        let t = &self.table_name;
        // Keep the entire schema transition atomic. A partial bootstrap can
        // otherwise leave a pod "ready" with missing ownership columns or
        // uniqueness guarantees after a transient DDL/permission failure.
        let mut tx = self.pool.begin().await.map_err(|e| {
            IronCrewError::Validation(format!("Failed to begin PostgreSQL bootstrap: {e}"))
        })?;
        self.lock_advisory(&mut tx, "bootstrap", "global", false)
            .await?;

        // 1. Create table if not exists
        let create_sql = format!(
            "CREATE TABLE IF NOT EXISTS {t} (
                run_id        TEXT PRIMARY KEY,
                flow_name     TEXT NOT NULL,
                flow          TEXT NOT NULL DEFAULT '',
                status        TEXT NOT NULL,
                started_at    TEXT NOT NULL,
                finished_at   TEXT NOT NULL,
                duration_ms   BIGINT NOT NULL,
                task_results  JSONB NOT NULL DEFAULT '[]',
                agent_count   INTEGER NOT NULL,
                task_count    INTEGER NOT NULL,
                total_tokens  INTEGER DEFAULT 0,
                cached_tokens INTEGER DEFAULT 0,
                tags          JSONB DEFAULT '[]',
                owner_instance_id TEXT NOT NULL DEFAULT '',
                lease_expires_at TEXT NOT NULL DEFAULT '',
                created_at    TIMESTAMPTZ DEFAULT NOW()
            )"
        );
        sqlx::query(sqlx::AssertSqlSafe(create_sql.to_string()))
            .execute(&mut *tx)
            .await
            .map_err(|e| IronCrewError::Validation(format!("Failed to create {t} table: {e}")))?;

        // 2. Add missing columns (heal older schema versions)
        let migrations: &[(&str, &str)] = &[
            (
                "flow",
                &format!("ALTER TABLE {t} ADD COLUMN IF NOT EXISTS flow TEXT NOT NULL DEFAULT ''"),
            ),
            (
                "total_tokens",
                &format!("ALTER TABLE {t} ADD COLUMN IF NOT EXISTS total_tokens INTEGER DEFAULT 0"),
            ),
            (
                "cached_tokens",
                &format!(
                    "ALTER TABLE {t} ADD COLUMN IF NOT EXISTS cached_tokens INTEGER DEFAULT 0"
                ),
            ),
            (
                "tags",
                &format!("ALTER TABLE {t} ADD COLUMN IF NOT EXISTS tags JSONB DEFAULT '[]'"),
            ),
            (
                "created_at",
                &format!(
                    "ALTER TABLE {t} ADD COLUMN IF NOT EXISTS created_at TIMESTAMPTZ DEFAULT NOW()"
                ),
            ),
        ];

        for (col, sql) in migrations {
            sqlx::query(sqlx::AssertSqlSafe(sql.to_string()))
                .execute(&mut *tx)
                .await
                .map_err(|e| {
                    IronCrewError::Validation(format!(
                        "Failed to migrate required run column '{col}': {e}"
                    ))
                })?;
        }

        // Ownership columns are safety-critical: continuing without them
        // would restore global startup abandonment semantics. Fail startup if
        // the database role cannot apply this backward-compatible migration.
        for (column, sql) in [
            (
                "owner_instance_id",
                format!(
                    "ALTER TABLE {t} ADD COLUMN IF NOT EXISTS owner_instance_id TEXT NOT NULL DEFAULT ''"
                ),
            ),
            (
                "lease_expires_at",
                format!(
                    "ALTER TABLE {t} ADD COLUMN IF NOT EXISTS lease_expires_at TEXT NOT NULL DEFAULT ''"
                ),
            ),
        ] {
            sqlx::query(sqlx::AssertSqlSafe(sql))
                .execute(&mut *tx)
                .await
                .map_err(|e| {
                    IronCrewError::Validation(format!(
                        "Failed to migrate required run ownership column '{}': {}",
                        column, e
                    ))
                })?;
        }

        // 3. Heal column types — upgrade TEXT to JSONB if needed
        let type_fixes: &[(&str, &str)] = &[
            ("task_results", &format!(
                "DO $$ BEGIN
                    IF EXISTS (
                        SELECT 1 FROM information_schema.columns
                        WHERE table_name = '{t}' AND column_name = 'task_results' AND data_type = 'text'
                    ) THEN
                        ALTER TABLE {t} ALTER COLUMN task_results TYPE JSONB USING task_results::jsonb;
                        RAISE NOTICE 'Upgraded task_results from TEXT to JSONB';
                    END IF;
                END $$"
            )),
            ("tags", &format!(
                "DO $$ BEGIN
                    IF EXISTS (
                        SELECT 1 FROM information_schema.columns
                        WHERE table_name = '{t}' AND column_name = 'tags' AND data_type = 'text'
                    ) THEN
                        ALTER TABLE {t} ALTER COLUMN tags TYPE JSONB USING tags::jsonb;
                        RAISE NOTICE 'Upgraded tags from TEXT to JSONB';
                    END IF;
                END $$"
            )),
        ];

        for (col, sql) in type_fixes {
            sqlx::query(sqlx::AssertSqlSafe(sql.to_string()))
                .execute(&mut *tx)
                .await
                .map_err(|e| {
                    IronCrewError::Validation(format!(
                        "Failed to migrate run column type '{col}': {e}"
                    ))
                })?;
        }

        // 4. Create indexes (IF NOT EXISTS — safe to run repeatedly)
        let indexes: &[&str] = &[
            &format!("CREATE INDEX IF NOT EXISTS idx_{t}_status ON {t} (status)"),
            &format!("CREATE INDEX IF NOT EXISTS idx_{t}_started_at ON {t} (started_at DESC)"),
            &format!("CREATE INDEX IF NOT EXISTS idx_{t}_flow_name ON {t} (flow_name)"),
            &format!("CREATE INDEX IF NOT EXISTS idx_{t}_tags ON {t} USING GIN (tags)"),
            &format!(
                "CREATE INDEX IF NOT EXISTS idx_{t}_task_results ON {t} USING GIN (task_results)"
            ),
            &format!(
                "CREATE INDEX IF NOT EXISTS idx_{t}_active_lease ON {t} (status, lease_expires_at)"
            ),
        ];

        for sql in indexes {
            sqlx::query(sqlx::AssertSqlSafe(sql.to_string()))
                .execute(&mut *tx)
                .await
                .map_err(|e| {
                    IronCrewError::Validation(format!("Failed to create run index: {e}"))
                })?;
        }

        // 5. Session tables — conversations and dialogs for resumable sessions
        let ct = &self.conversations_table;
        let dt = &self.dialogs_table;

        let session_tables = [
            format!(
                "CREATE TABLE IF NOT EXISTS {ct} (
                    id          TEXT PRIMARY KEY,
                    flow_name   TEXT NOT NULL,
                    agent_name  TEXT NOT NULL,
                    execution   JSONB NOT NULL DEFAULT '{{}}',
                    messages    JSONB NOT NULL DEFAULT '[]',
                    created_at  TEXT NOT NULL,
                    updated_at  TEXT NOT NULL,
                    revision    BIGINT NOT NULL DEFAULT 0
                )"
            ),
            format!(
                "CREATE TABLE IF NOT EXISTS {dt} (
                    id          TEXT PRIMARY KEY,
                    flow_name   TEXT NOT NULL,
                    agent_names JSONB NOT NULL DEFAULT '[]',
                    starter     TEXT NOT NULL,
                    transcript  JSONB NOT NULL DEFAULT '[]',
                    next_index  INTEGER NOT NULL,
                    stopped     BOOLEAN NOT NULL DEFAULT FALSE,
                    stop_reason TEXT,
                    created_at  TEXT NOT NULL,
                    updated_at  TEXT NOT NULL,
                    revision    BIGINT NOT NULL DEFAULT 0
                )"
            ),
        ];
        for sql in &session_tables {
            sqlx::query(sqlx::AssertSqlSafe(sql.to_string()))
                .execute(&mut *tx)
                .await
                .map_err(|e| {
                    IronCrewError::Validation(format!("Failed to create session table: {}", e))
                })?;
        }

        // Add flow_path column for schemas predating Phase-1 HITL support.
        // Guarded with IF NOT EXISTS for idempotency (matches the pattern
        // used for total_tokens / cached_tokens / tags above).
        let session_migrations: &[(&str, String)] = &[
            (
                "conversations.flow_path",
                format!("ALTER TABLE {ct} ADD COLUMN IF NOT EXISTS flow_path TEXT"),
            ),
            (
                "dialogs.flow_path",
                format!("ALTER TABLE {dt} ADD COLUMN IF NOT EXISTS flow_path TEXT"),
            ),
            (
                "conversations.revision",
                format!(
                    "ALTER TABLE {ct} ADD COLUMN IF NOT EXISTS revision BIGINT NOT NULL DEFAULT 0"
                ),
            ),
            (
                "conversations.execution",
                format!(
                    "ALTER TABLE {ct} ADD COLUMN IF NOT EXISTS execution JSONB NOT NULL DEFAULT '{{}}'"
                ),
            ),
            (
                "dialogs.revision",
                format!(
                    "ALTER TABLE {dt} ADD COLUMN IF NOT EXISTS revision BIGINT NOT NULL DEFAULT 0"
                ),
            ),
        ];
        for (label, sql) in session_migrations {
            sqlx::query(sqlx::AssertSqlSafe(sql.to_string()))
                .execute(&mut *tx)
                .await
                .map_err(|e| {
                    IronCrewError::Validation(format!(
                        "Failed to migrate session column '{label}': {e}"
                    ))
                })?;
        }

        // Enforce the documented `(flow_path, id)` uniqueness for sessions.
        // Earlier versions used `id` as the sole PRIMARY KEY, which meant a
        // save from flow-B would overwrite flow-A's session with the same
        // id. PostgreSQL 15+ is required so we can use `NULLS NOT DISTINCT`
        // and preserve deterministic uniqueness for legacy `flow_path IS NULL`
        // rows as well.
        let session_unique_indexes: &[(&str, String)] = &[
            (
                "conversations: composite unique (flow_path, id)",
                format!(
                    "CREATE UNIQUE INDEX IF NOT EXISTS uniq_{ct}_flow_id \
                     ON {ct} (flow_path, id) NULLS NOT DISTINCT"
                ),
            ),
            (
                "dialogs: composite unique (flow_path, id)",
                format!(
                    "CREATE UNIQUE INDEX IF NOT EXISTS uniq_{dt}_flow_id \
                     ON {dt} (flow_path, id) NULLS NOT DISTINCT"
                ),
            ),
        ];
        for (label, sql) in session_unique_indexes {
            sqlx::query(sqlx::AssertSqlSafe(sql.to_string()))
                .execute(&mut *tx)
                .await
                .map_err(|e| {
                    IronCrewError::Validation(format!(
                        "Failed to enforce session uniqueness '{label}': {e}"
                    ))
                })?;
        }

        // `CREATE INDEX IF NOT EXISTS` accepts a same-named but incompatible
        // index. Verify the exact safety properties before dropping the old
        // primary keys; a mismatch rolls the entire transaction back.
        for (table, index) in [
            (ct.as_str(), format!("uniq_{ct}_flow_id")),
            (dt.as_str(), format!("uniq_{dt}_flow_id")),
        ] {
            let valid: bool = sqlx::query_scalar(
                "SELECT EXISTS (\
                    SELECT 1 \
                    FROM pg_index i \
                    JOIN pg_class idx ON idx.oid = i.indexrelid \
                    JOIN pg_class tbl ON tbl.oid = i.indrelid \
                    JOIN pg_namespace ns ON ns.oid = tbl.relnamespace \
                    WHERE ns.nspname = current_schema() \
                      AND tbl.relname = $1 AND idx.relname = $2 \
                      AND i.indisunique AND i.indnullsnotdistinct \
                      AND i.indnkeyatts = 2 \
                      AND pg_get_indexdef(i.indexrelid, 1, TRUE) = 'flow_path' \
                      AND pg_get_indexdef(i.indexrelid, 2, TRUE) = 'id'\
                )",
            )
            .bind(table)
            .bind(&index)
            .fetch_one(&mut *tx)
            .await
            .map_err(|e| {
                IronCrewError::Validation(format!(
                    "Failed to verify new PostgreSQL session index '{index}': {e}"
                ))
            })?;
            if !valid {
                return Err(IronCrewError::Validation(format!(
                    "PostgreSQL index '{index}' exists without the required UNIQUE NULLS NOT DISTINCT (flow_path, id) properties"
                )));
            }
        }

        for (label, sql) in [
            (
                "conversations: drop legacy id PK",
                format!("ALTER TABLE {ct} DROP CONSTRAINT IF EXISTS {ct}_pkey"),
            ),
            (
                "dialogs: drop legacy id PK",
                format!("ALTER TABLE {dt} DROP CONSTRAINT IF EXISTS {dt}_pkey"),
            ),
        ] {
            sqlx::query(sqlx::AssertSqlSafe(sql))
                .execute(&mut *tx)
                .await
                .map_err(|e| {
                    IronCrewError::Validation(format!(
                        "Failed to remove legacy session constraint '{label}': {e}"
                    ))
                })?;
        }

        // Session indexes — updated_at helps "list recent sessions" queries
        let session_indexes = [
            format!("CREATE INDEX IF NOT EXISTS idx_{ct}_updated_at ON {ct} (updated_at DESC)"),
            format!("CREATE INDEX IF NOT EXISTS idx_{ct}_flow_name ON {ct} (flow_name)"),
            format!("CREATE INDEX IF NOT EXISTS idx_{ct}_flow_path ON {ct} (flow_path)"),
            format!("CREATE INDEX IF NOT EXISTS idx_{dt}_updated_at ON {dt} (updated_at DESC)"),
            format!("CREATE INDEX IF NOT EXISTS idx_{dt}_flow_name ON {dt} (flow_name)"),
            format!("CREATE INDEX IF NOT EXISTS idx_{dt}_flow_path ON {dt} (flow_path)"),
        ];
        for sql in &session_indexes {
            sqlx::query(sqlx::AssertSqlSafe(sql.to_string()))
                .execute(&mut *tx)
                .await
                .map_err(|e| {
                    IronCrewError::Validation(format!("Failed to create session index: {e}"))
                })?;
        }

        // 6. Audit events table
        let at = &self.audit_events_table;
        let audit_sql = format!(
            "CREATE TABLE IF NOT EXISTS {at} (
                id          TEXT PRIMARY KEY,
                timestamp   TEXT NOT NULL,
                action      TEXT NOT NULL,
                flow_path   TEXT,
                target      TEXT,
                actor       TEXT,
                source_ip   TEXT,
                success     BOOLEAN NOT NULL,
                status_code INTEGER NOT NULL,
                metadata    JSONB
            )"
        );
        sqlx::query(sqlx::AssertSqlSafe(audit_sql.to_string()))
            .execute(&mut *tx)
            .await
            .map_err(|e| IronCrewError::Validation(format!("Failed to create {at} table: {e}")))?;

        let audit_indexes: &[String] = &[
            format!("CREATE INDEX IF NOT EXISTS idx_{at}_timestamp_desc ON {at} (timestamp DESC)"),
            format!("CREATE INDEX IF NOT EXISTS idx_{at}_flow_path ON {at} (flow_path)"),
        ];
        for sql in audit_indexes {
            sqlx::query(sqlx::AssertSqlSafe(sql.to_string()))
                .execute(&mut *tx)
                .await
                .map_err(|e| {
                    IronCrewError::Validation(format!("Failed to create audit index: {e}"))
                })?;
        }

        // 7. Durable request idempotency. Keep identifiers derived solely
        // from the validated table prefix; the compact suffixes also keep
        // every index name below PostgreSQL's 63-byte identifier limit at
        // the maximum supported prefix length.
        let it = &self.idempotency_table;
        let idempotency_sql = format!(
            "CREATE TABLE IF NOT EXISTS {it} (
                key_hash            TEXT PRIMARY KEY,
                principal_id        TEXT NOT NULL,
                request_fingerprint TEXT NOT NULL,
                operation           TEXT NOT NULL,
                scope               TEXT NOT NULL,
                resource_id         TEXT NOT NULL,
                exclusive_scope     TEXT,
                attempt_id          TEXT NOT NULL,
                owner_instance_id   TEXT NOT NULL,
                base_revision       BIGINT,
                state               TEXT NOT NULL,
                response_status     INTEGER,
                response_body       TEXT,
                lease_expires_at    TEXT NOT NULL,
                created_at          TEXT NOT NULL,
                updated_at          TEXT NOT NULL,
                completed_at        TEXT,
                expires_at          TEXT,
                cancel_requested_at TEXT,
                owner_draining_at   TEXT,
                ttl_seconds         BIGINT NOT NULL,
                CHECK (state IN ('claimed', 'running', 'completed', 'indeterminate')),
                CHECK (response_status IS NULL OR response_status BETWEEN 100 AND 599),
                CHECK (ttl_seconds > 0)
            )"
        );
        sqlx::query(sqlx::AssertSqlSafe(idempotency_sql))
            .execute(&mut *tx)
            .await
            .map_err(|e| {
                IronCrewError::Validation(format!(
                    "Failed to create PostgreSQL idempotency table '{it}': {e}"
                ))
            })?;

        // Cross-replica run cancellation uses the keyed run ledger as a
        // durable mailbox. This nullable timestamp is intentionally separate
        // from the replay response so existing clients and ledgers remain
        // backwards compatible.
        let add_cancel_requested =
            format!("ALTER TABLE {it} ADD COLUMN IF NOT EXISTS cancel_requested_at TEXT");
        sqlx::query(sqlx::AssertSqlSafe(add_cancel_requested))
            .execute(&mut *tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "Failed to add PostgreSQL idempotent-run cancellation column: {error}"
                ))
            })?;

        // Process drain is fenced on each exact in-flight run ledger. A
        // nullable timestamp preserves old rows while preventing a coarse
        // instance marker from poisoning a later process that reuses a name.
        let add_owner_draining =
            format!("ALTER TABLE {it} ADD COLUMN IF NOT EXISTS owner_draining_at TEXT");
        sqlx::query(sqlx::AssertSqlSafe(add_owner_draining))
            .execute(&mut *tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "Failed to add PostgreSQL idempotent-run owner-drain column: {error}"
                ))
            })?;

        // Backfill ledgers created before principal-aware admission. The
        // opaque legacy digest preserves their non-reusability across an
        // upgrade without persisting a bearer credential or raw label.
        let add_principal = format!("ALTER TABLE {it} ADD COLUMN IF NOT EXISTS principal_id TEXT");
        sqlx::query(sqlx::AssertSqlSafe(add_principal))
            .execute(&mut *tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "Failed to add PostgreSQL idempotency principal column: {error}"
                ))
            })?;
        let backfill_principal = format!(
            "UPDATE {it} SET principal_id = $1 \
             WHERE principal_id IS NULL OR principal_id = ''"
        );
        sqlx::query(sqlx::AssertSqlSafe(backfill_principal))
            .bind(PrincipalId::legacy().as_str())
            .execute(&mut *tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "Failed to migrate PostgreSQL idempotency principals: {error}"
                ))
            })?;
        let require_principal = format!("ALTER TABLE {it} ALTER COLUMN principal_id SET NOT NULL");
        sqlx::query(sqlx::AssertSqlSafe(require_principal))
            .execute(&mut *tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "Failed to require PostgreSQL idempotency principals: {error}"
                ))
            })?;
        let principal_constraint = format!("{it}_principal_ck");
        let has_principal_constraint: bool = sqlx::query_scalar(
            "SELECT EXISTS (\
                 SELECT 1 FROM pg_constraint AS con \
                 JOIN pg_class AS tbl ON tbl.oid = con.conrelid \
                 JOIN pg_namespace AS ns ON ns.oid = tbl.relnamespace \
                 WHERE ns.nspname = current_schema() AND tbl.relname = $1 \
                   AND con.conname = $2 AND con.contype = 'c'\
             )",
        )
        .bind(it)
        .bind(&principal_constraint)
        .fetch_one(&mut *tx)
        .await
        .map_err(|error| {
            IronCrewError::Validation(format!(
                "Failed to inspect PostgreSQL idempotency principal constraint: {error}"
            ))
        })?;
        if !has_principal_constraint {
            let sql = format!(
                "ALTER TABLE {it} ADD CONSTRAINT {principal_constraint} \
                 CHECK (length(principal_id) = 64 AND principal_id !~ '[^0-9a-f]')"
            );
            sqlx::query(sqlx::AssertSqlSafe(sql))
                .execute(&mut *tx)
                .await
                .map_err(|error| {
                    IronCrewError::Validation(format!(
                        "Failed to constrain PostgreSQL idempotency principals: {error}"
                    ))
                })?;
        }

        let idempotency_indexes = [
            format!("CREATE INDEX IF NOT EXISTS {it}_exp_idx ON {it} (expires_at)"),
            format!(
                "CREATE INDEX IF NOT EXISTS {it}_res_idx \
                 ON {it} (operation, scope, resource_id)"
            ),
            format!(
                "CREATE INDEX IF NOT EXISTS {it}_lease_idx \
                 ON {it} (operation, lease_expires_at, key_hash) \
                 WHERE state IN ('claimed', 'running')"
            ),
            format!(
                "CREATE UNIQUE INDEX IF NOT EXISTS {it}_scope_uidx \
                 ON {it} (exclusive_scope) \
                 WHERE exclusive_scope IS NOT NULL \
                   AND state IN ('claimed', 'running')"
            ),
            format!(
                "CREATE INDEX IF NOT EXISTS {it}_owner_idx \
                 ON {it} (owner_instance_id, operation) \
                 WHERE state IN ('claimed', 'running')"
            ),
        ];
        for sql in &idempotency_indexes {
            sqlx::query(sqlx::AssertSqlSafe(sql.clone()))
                .execute(&mut *tx)
                .await
                .map_err(|e| {
                    IronCrewError::Validation(format!(
                        "Failed to create PostgreSQL idempotency index: {e}"
                    ))
                })?;
        }

        // A compact accounting table avoids COUNT/SUM scans on every claim
        // and completion. The global row is always updated first, then the
        // opaque principal row, matching the application advisory-lock order.
        let accounting = &self.idempotency_accounting_table;
        let accounting_sql = format!(
            "CREATE TABLE IF NOT EXISTS {accounting} (
                principal_id    TEXT PRIMARY KEY,
                is_global       BOOLEAN NOT NULL,
                record_count    BIGINT NOT NULL DEFAULT 0 CHECK (record_count >= 0),
                in_flight_count BIGINT NOT NULL DEFAULT 0 CHECK (in_flight_count >= 0),
                response_bytes  BIGINT NOT NULL DEFAULT 0 CHECK (response_bytes >= 0),
                updated_at      TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
                CHECK ((is_global AND principal_id = 'global') OR
                       (NOT is_global AND length(principal_id) = 64 AND
                        principal_id !~ '[^0-9a-f]'))
            );
            INSERT INTO {accounting} (principal_id, is_global)
            VALUES ('global', TRUE)
            ON CONFLICT (principal_id) DO NOTHING"
        );
        sqlx::raw_sql(sqlx::AssertSqlSafe(accounting_sql))
            .execute(&mut *tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "Failed to create PostgreSQL idempotency accounting table: {error}"
                ))
            })?;

        let accounting_function = format!("{it}_acct_fn");
        let accounting_trigger = format!("{it}_acct_trg");
        let function_sql = format!(
            r#"CREATE OR REPLACE FUNCTION {accounting_function}() RETURNS TRIGGER
               LANGUAGE plpgsql AS $ironcrew$
               DECLARE
                   changed_principal TEXT;
                   record_delta BIGINT := 0;
                   in_flight_delta BIGINT := 0;
                   response_delta BIGINT := 0;
               BEGIN
                   IF TG_OP = 'INSERT' THEN
                       changed_principal := NEW.principal_id;
                       record_delta := 1;
                       in_flight_delta := CASE WHEN NEW.state IN ('claimed', 'running') THEN 1 ELSE 0 END;
                       response_delta := COALESCE(octet_length(NEW.response_body), 0);
                   ELSIF TG_OP = 'DELETE' THEN
                       changed_principal := OLD.principal_id;
                       record_delta := -1;
                       in_flight_delta := -(CASE WHEN OLD.state IN ('claimed', 'running') THEN 1 ELSE 0 END);
                       response_delta := -COALESCE(octet_length(OLD.response_body), 0);
                   ELSE
                       IF OLD.principal_id <> NEW.principal_id THEN
                           RAISE EXCEPTION 'idempotency principal_id is immutable';
                       END IF;
                       changed_principal := NEW.principal_id;
                       in_flight_delta :=
                           (CASE WHEN NEW.state IN ('claimed', 'running') THEN 1 ELSE 0 END) -
                           (CASE WHEN OLD.state IN ('claimed', 'running') THEN 1 ELSE 0 END);
                       response_delta := COALESCE(octet_length(NEW.response_body), 0) -
                                         COALESCE(octet_length(OLD.response_body), 0);
                       IF in_flight_delta = 0 AND response_delta = 0 THEN
                           RETURN NEW;
                       END IF;
                   END IF;

                   UPDATE {accounting}
                   SET record_count = record_count + record_delta,
                       in_flight_count = in_flight_count + in_flight_delta,
                       response_bytes = response_bytes + response_delta,
                       updated_at = clock_timestamp()
                   WHERE principal_id = 'global' AND is_global = TRUE;
                   IF NOT FOUND THEN
                       RAISE EXCEPTION 'global idempotency accounting row is missing';
                   END IF;

                   IF TG_OP = 'INSERT' THEN
                       INSERT INTO {accounting} AS usage
                           (principal_id, is_global, record_count, in_flight_count,
                            response_bytes, updated_at)
                       VALUES (changed_principal, FALSE, record_delta, in_flight_delta,
                               response_delta, clock_timestamp())
                       ON CONFLICT (principal_id) DO UPDATE SET
                           record_count = usage.record_count + EXCLUDED.record_count,
                           in_flight_count = usage.in_flight_count + EXCLUDED.in_flight_count,
                           response_bytes = usage.response_bytes + EXCLUDED.response_bytes,
                           updated_at = clock_timestamp();
                   ELSE
                       UPDATE {accounting}
                       SET record_count = record_count + record_delta,
                           in_flight_count = in_flight_count + in_flight_delta,
                           response_bytes = response_bytes + response_delta,
                           updated_at = clock_timestamp()
                       WHERE principal_id = changed_principal AND is_global = FALSE;
                       IF NOT FOUND THEN
                           RAISE EXCEPTION 'principal idempotency accounting row is missing';
                       END IF;
                   END IF;

                   DELETE FROM {accounting}
                   WHERE principal_id = changed_principal AND is_global = FALSE
                     AND record_count = 0 AND in_flight_count = 0 AND response_bytes = 0;
                   IF TG_OP = 'DELETE' THEN
                       RETURN OLD;
                   END IF;
                   RETURN NEW;
               END;
               $ironcrew$"#
        );
        sqlx::query(sqlx::AssertSqlSafe(function_sql))
            .execute(&mut *tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "Failed to create PostgreSQL idempotency accounting function: {error}"
                ))
            })?;
        let trigger_sql = format!(
            "DROP TRIGGER IF EXISTS {accounting_trigger} ON {it}; \
             CREATE TRIGGER {accounting_trigger} \
             AFTER INSERT OR DELETE OR UPDATE OF principal_id, state, response_body ON {it} \
             FOR EACH ROW EXECUTE FUNCTION {accounting_function}()"
        );
        sqlx::raw_sql(sqlx::AssertSqlSafe(trigger_sql))
            .execute(&mut *tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "Failed to create PostgreSQL idempotency accounting trigger: {error}"
                ))
            })?;

        // Reconcile once during migration. DDL locks on the ledger remain
        // held until commit, so a concurrent write either precedes this scan
        // or runs through the newly installed trigger afterwards.
        let reconcile_accounting = format!(
            "DELETE FROM {accounting} WHERE is_global = FALSE; \
             INSERT INTO {accounting} \
                 (principal_id, is_global, record_count, in_flight_count, response_bytes) \
             SELECT principal_id, FALSE, COUNT(*)::BIGINT, \
                    COUNT(*) FILTER (WHERE state IN ('claimed', 'running'))::BIGINT, \
                    COALESCE(SUM(octet_length(response_body)), 0)::BIGINT \
             FROM {it} GROUP BY principal_id; \
             UPDATE {accounting} SET \
                 record_count = (SELECT COUNT(*)::BIGINT FROM {it}), \
                 in_flight_count = (SELECT COUNT(*)::BIGINT FROM {it} \
                                    WHERE state IN ('claimed', 'running')), \
                 response_bytes = (SELECT COALESCE(SUM(octet_length(response_body)), 0)::BIGINT \
                                   FROM {it}), \
                 updated_at = clock_timestamp() \
             WHERE principal_id = 'global' AND is_global = TRUE"
        );
        sqlx::raw_sql(sqlx::AssertSqlSafe(reconcile_accounting))
            .execute(&mut *tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "Failed to reconcile PostgreSQL idempotency accounting: {error}"
                ))
            })?;

        // 8. Durable human-input mailbox. Question metadata and answers are
        // application-encrypted before they enter SQL; only routing/fencing
        // fields remain queryable. The run FK is both a safety net and the
        // final cleanup path for explicit run deletion.
        let hit = &self.human_inputs_table;
        let human_inputs_sql = format!(
            "CREATE TABLE IF NOT EXISTS {hit} (
                run_id                    TEXT NOT NULL,
                question_id               TEXT NOT NULL,
                flow                      TEXT NOT NULL,
                owner_instance_id         TEXT NOT NULL,
                key_hash                  TEXT NOT NULL,
                attempt_id                TEXT NOT NULL,
                question_digest           TEXT NOT NULL,
                question_key_fingerprint  TEXT NOT NULL,
                question_nonce            BYTEA NOT NULL,
                question_ciphertext       BYTEA NOT NULL,
                answer_key_fingerprint    TEXT,
                answer_nonce              BYTEA,
                answer_ciphertext         BYTEA,
                state                     TEXT NOT NULL DEFAULT 'pending',
                created_at                TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
                expires_at                TIMESTAMPTZ NOT NULL,
                answered_at               TIMESTAMPTZ,
                PRIMARY KEY (run_id, question_id),
                CONSTRAINT {hit}_run_fk FOREIGN KEY (run_id)
                    REFERENCES {t} (run_id) ON DELETE CASCADE,
                CONSTRAINT {hit}_state_ck CHECK (state IN ('pending', 'answered')),
                CONSTRAINT {hit}_payload_ck CHECK (
                    octet_length(question_nonce) > 0 AND
                    octet_length(question_ciphertext) > 0 AND
                    length(question_key_fingerprint) > 0 AND
                    length(question_digest) = 64 AND
                    question_digest !~ '[^0-9a-f]' AND
                    ((state = 'pending' AND answer_key_fingerprint IS NULL AND
                      answer_nonce IS NULL AND answer_ciphertext IS NULL AND
                      answered_at IS NULL) OR
                     (state = 'answered' AND answer_key_fingerprint IS NOT NULL AND
                      answer_nonce IS NOT NULL AND answer_ciphertext IS NOT NULL AND
                      answered_at IS NOT NULL))
                ),
                CONSTRAINT {hit}_expiry_ck CHECK (expires_at > created_at)
            )"
        );
        sqlx::query(sqlx::AssertSqlSafe(human_inputs_sql))
            .execute(&mut *tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "Failed to create PostgreSQL human-input mailbox table '{hit}': {error}"
                ))
            })?;
        // Question AAD gained a semantic digest after the first mailbox
        // rollout. Old rows cannot be authenticated under the new AAD and are
        // intentionally discarded instead of being silently reinterpreted.
        let add_question_digest =
            format!("ALTER TABLE {hit} ADD COLUMN IF NOT EXISTS question_digest TEXT");
        sqlx::query(sqlx::AssertSqlSafe(add_question_digest))
            .execute(&mut *tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "Failed to add PostgreSQL human-input question digest: {error}"
                ))
            })?;
        let discard_legacy_questions = format!("DELETE FROM {hit} WHERE question_digest IS NULL");
        sqlx::query(sqlx::AssertSqlSafe(discard_legacy_questions))
            .execute(&mut *tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "Failed to discard legacy PostgreSQL human-input rows: {error}"
                ))
            })?;
        let require_question_digest =
            format!("ALTER TABLE {hit} ALTER COLUMN question_digest SET NOT NULL");
        sqlx::query(sqlx::AssertSqlSafe(require_question_digest))
            .execute(&mut *tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "Failed to require PostgreSQL human-input question digest: {error}"
                ))
            })?;
        let refresh_human_payload_constraint = format!(
            "ALTER TABLE {hit} DROP CONSTRAINT IF EXISTS {hit}_payload_ck; \
             ALTER TABLE {hit} ADD CONSTRAINT {hit}_payload_ck CHECK (\
                 octet_length(question_nonce) > 0 AND \
                 octet_length(question_ciphertext) > 0 AND \
                 length(question_key_fingerprint) > 0 AND \
                 length(question_digest) = 64 AND \
                 question_digest !~ '[^0-9a-f]' AND \
                 ((state = 'pending' AND answer_key_fingerprint IS NULL AND \
                   answer_nonce IS NULL AND answer_ciphertext IS NULL AND \
                   answered_at IS NULL) OR \
                  (state = 'answered' AND answer_key_fingerprint IS NOT NULL AND \
                   answer_nonce IS NOT NULL AND answer_ciphertext IS NOT NULL AND \
                   answered_at IS NOT NULL))\
             )"
        );
        sqlx::raw_sql(sqlx::AssertSqlSafe(refresh_human_payload_constraint))
            .execute(&mut *tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "Failed to refresh PostgreSQL human-input payload constraint: {error}"
                ))
            })?;
        for sql in [
            format!(
                "CREATE INDEX IF NOT EXISTS {hit}_run_idx ON {hit} (run_id, expires_at) \
                 WHERE state = 'pending'"
            ),
            format!("CREATE INDEX IF NOT EXISTS {hit}_exp_idx ON {hit} (expires_at)"),
            format!(
                "CREATE INDEX IF NOT EXISTS {hit}_pex_idx \
                 ON {hit} (expires_at, run_id, question_id) \
                 WHERE state = 'pending'"
            ),
        ] {
            sqlx::query(sqlx::AssertSqlSafe(sql))
                .execute(&mut *tx)
                .await
                .map_err(|error| {
                    IronCrewError::Validation(format!(
                        "Failed to create PostgreSQL human-input mailbox index: {error}"
                    ))
                })?;
        }

        // 9. Durable bounded run-event journal. The event table owns payloads,
        // the state table keeps exact per-run replay bounds, and one trigger-
        // maintained singleton accounts global rows/bytes even during
        // cascading run deletion.
        let events = &self.run_events_table;
        let event_state = &self.run_event_state_table;
        let event_usage = &self.run_event_usage_table;
        let run_events_sql = format!(
            "CREATE TABLE IF NOT EXISTS {events} (
                run_id       TEXT NOT NULL,
                sequence     BIGINT NOT NULL,
                event_type   TEXT NOT NULL,
                payload      JSONB NOT NULL,
                payload_bytes BIGINT NOT NULL,
                accounted_bytes BIGINT GENERATED ALWAYS AS (
                    GREATEST(payload_bytes, octet_length(payload::text)::BIGINT,
                             {MIN_ACCOUNTED_RUN_EVENT_BYTES})
                ) STORED,
                created_at   TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
                expires_at   TIMESTAMPTZ NOT NULL,
                PRIMARY KEY (run_id, sequence),
                CONSTRAINT {events}_run_fk FOREIGN KEY (run_id)
                    REFERENCES {t} (run_id) ON DELETE CASCADE,
                CONSTRAINT {events}_payload_ck CHECK (
                    sequence > 0 AND
                    length(event_type) BETWEEN 1 AND 64 AND
                    event_type !~ '[^a-z0-9_]' AND
                    jsonb_typeof(payload) = 'object' AND
                    payload ? 'event' AND payload->>'event' = event_type AND
                    payload_bytes > 0 AND
                    accounted_bytes > 0 AND
                    accounted_bytes <= {HARD_MAX_EVENT_BYTES}
                ),
                CONSTRAINT {events}_expiry_ck CHECK (expires_at > created_at)
            );
            CREATE TABLE IF NOT EXISTS {event_state} (
                run_id                  TEXT PRIMARY KEY,
                flow                    TEXT NOT NULL,
                owner_instance_id       TEXT NOT NULL,
                latest_sequence         BIGINT NOT NULL DEFAULT 0,
                dropped_through         BIGINT NOT NULL DEFAULT 0,
                retained_events         BIGINT NOT NULL DEFAULT 0,
                retained_bytes          BIGINT NOT NULL DEFAULT 0,
                journal_complete        BOOLEAN NOT NULL DEFAULT TRUE,
                eviction_reason         TEXT,
                terminal_event_sequence BIGINT,
                updated_at              TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
                CONSTRAINT {event_state}_run_fk FOREIGN KEY (run_id)
                    REFERENCES {t} (run_id) ON DELETE CASCADE,
                CONSTRAINT {event_state}_bounds_ck CHECK (
                    latest_sequence >= 0 AND dropped_through >= 0 AND
                    dropped_through <= latest_sequence AND
                    retained_events >= 0 AND retained_bytes >= 0 AND
                    ((retained_events = 0 AND retained_bytes = 0) OR
                     (retained_events > 0 AND retained_bytes > 0)) AND
                    (terminal_event_sequence IS NULL OR
                     (terminal_event_sequence > 0 AND
                      terminal_event_sequence <= latest_sequence))
                ),
                CONSTRAINT {event_state}_reason_ck CHECK (
                    (dropped_through = 0 AND eviction_reason IS NULL) OR
                    (dropped_through > 0 AND eviction_reason IN
                        ('writer_backpressure', 'retention', 'global_capacity', 'owner_lost'))
                )
            );
            CREATE TABLE IF NOT EXISTS {event_usage} (
                singleton       BOOLEAN PRIMARY KEY DEFAULT TRUE CHECK (singleton),
                schema_version  INTEGER NOT NULL DEFAULT 0,
                retained_events BIGINT NOT NULL DEFAULT 0,
                retained_bytes  BIGINT NOT NULL DEFAULT 0,
                updated_at      TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
                CONSTRAINT {event_usage}_usage_ck CHECK (
                    schema_version >= 0 AND
                    retained_events >= 0 AND retained_bytes >= 0 AND
                    ((retained_events = 0 AND retained_bytes = 0) OR
                     (retained_events > 0 AND retained_bytes > 0))
                )
            );
            INSERT INTO {event_usage} (singleton) VALUES (TRUE)
            ON CONFLICT (singleton) DO NOTHING"
        );
        sqlx::raw_sql(sqlx::AssertSqlSafe(run_events_sql))
            .execute(&mut *tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "Failed to create PostgreSQL run-event journal tables: {error}"
                ))
            })?;
        let run_event_usage_migration = format!(
            "ALTER TABLE {event_usage} ADD COLUMN IF NOT EXISTS \
                 schema_version INTEGER NOT NULL DEFAULT 0"
        );
        sqlx::query(sqlx::AssertSqlSafe(run_event_usage_migration))
            .execute(&mut *tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "Failed to migrate PostgreSQL run-event schema version: {error}"
                ))
            })?;

        // `payload_bytes` is the storage-neutral compact JSON wire size used
        // for duplicate and page semantics. PostgreSQL's JSONB rendering can
        // be larger, so a generated conservative accounting size prevents a
        // direct/buggy insert from understating memory/storage consumption.
        // Refresh the constraint for databases created by an earlier binary.
        let run_event_payload_accounting_sql = format!(
            "ALTER TABLE {events} ADD COLUMN IF NOT EXISTS accounted_bytes BIGINT \
                 GENERATED ALWAYS AS (\
                     GREATEST(payload_bytes, octet_length(payload::text)::BIGINT, \
                              {MIN_ACCOUNTED_RUN_EVENT_BYTES})\
                 ) STORED; \
             ALTER TABLE {events} DROP CONSTRAINT IF EXISTS {events}_payload_ck; \
             ALTER TABLE {events} ADD CONSTRAINT {events}_payload_ck CHECK (\
                 sequence > 0 AND \
                 length(event_type) BETWEEN 1 AND 64 AND \
                 event_type !~ '[^a-z0-9_]' AND \
                 jsonb_typeof(payload) = 'object' AND \
                 payload ? 'event' AND payload->>'event' = event_type AND \
                 payload_bytes > 0 AND accounted_bytes > 0 AND \
                 accounted_bytes <= {HARD_MAX_EVENT_BYTES}\
             )"
        );
        sqlx::raw_sql(sqlx::AssertSqlSafe(run_event_payload_accounting_sql))
            .execute(&mut *tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "Failed to harden PostgreSQL run-event payload accounting: {error}"
                ))
            })?;

        for sql in [
            format!(
                "CREATE INDEX IF NOT EXISTS {events}_exp_idx ON {events} \
                 (expires_at, run_id, sequence)"
            ),
            format!(
                "CREATE INDEX IF NOT EXISTS {events}_old_idx ON {events} \
                 (created_at, run_id, sequence)"
            ),
        ] {
            sqlx::query(sqlx::AssertSqlSafe(sql))
                .execute(&mut *tx)
                .await
                .map_err(|error| {
                    IronCrewError::Validation(format!(
                        "Failed to create PostgreSQL run-event journal index: {error}"
                    ))
                })?;
        }

        let event_usage_function = format!("{events}_acct_fn");
        let event_usage_trigger = format!("{events}_acct_trg");
        let event_usage_function_sql = format!(
            r#"CREATE OR REPLACE FUNCTION {event_usage_function}() RETURNS TRIGGER
               LANGUAGE plpgsql AS $ironcrew$
               BEGIN
                   IF TG_OP = 'INSERT' THEN
                       UPDATE {event_usage}
                       SET retained_events = retained_events + 1,
                           retained_bytes = retained_bytes + NEW.accounted_bytes,
                           updated_at = clock_timestamp()
                       WHERE singleton = TRUE;
                       IF NOT FOUND THEN
                           RAISE EXCEPTION 'global run-event accounting row is missing';
                       END IF;
                       RETURN NEW;
                   END IF;

                   UPDATE {event_usage}
                   SET retained_events = retained_events - 1,
                       retained_bytes = retained_bytes - OLD.accounted_bytes,
                       updated_at = clock_timestamp()
                   WHERE singleton = TRUE;
                   IF NOT FOUND THEN
                       RAISE EXCEPTION 'global run-event accounting row is missing';
                   END IF;
                   RETURN OLD;
               END;
               $ironcrew$"#
        );
        sqlx::query(sqlx::AssertSqlSafe(event_usage_function_sql))
            .execute(&mut *tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "Failed to create PostgreSQL run-event accounting function: {error}"
                ))
            })?;
        let event_usage_trigger_sql = format!(
            "DROP TRIGGER IF EXISTS {event_usage_trigger} ON {events}; \
             CREATE TRIGGER {event_usage_trigger} \
             AFTER INSERT OR DELETE ON {events} FOR EACH ROW \
             EXECUTE FUNCTION {event_usage_function}()"
        );
        sqlx::raw_sql(sqlx::AssertSqlSafe(event_usage_trigger_sql))
            .execute(&mut *tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "Failed to create PostgreSQL run-event accounting trigger: {error}"
                ))
            })?;

        // Reconcile any journal created by a previous binary before releasing
        // the bootstrap DDL locks. Existing dropped boundaries remain intact;
        // retained counts and global usage are rebuilt from source rows.
        let reconcile_run_events = format!(
            "INSERT INTO {event_state} AS state (
                 run_id, flow, owner_instance_id, latest_sequence,
                 retained_events, retained_bytes, journal_complete,
                 terminal_event_sequence, updated_at
             )
             SELECT event.run_id, run.flow, run.owner_instance_id,
                    MAX(event.sequence), COUNT(*)::BIGINT,
                    SUM(event.accounted_bytes)::BIGINT,
                    MIN(event.sequence) = 1 AND COUNT(*)::BIGINT = MAX(event.sequence),
                    MAX(event.sequence) FILTER (WHERE event.event_type = 'run_complete'),
                    clock_timestamp()
             FROM {events} AS event
             JOIN {t} AS run ON run.run_id = event.run_id
             GROUP BY event.run_id, run.flow, run.owner_instance_id
             ON CONFLICT (run_id) DO UPDATE SET
                 flow = EXCLUDED.flow,
                 owner_instance_id = EXCLUDED.owner_instance_id,
                 latest_sequence = GREATEST(state.latest_sequence, EXCLUDED.latest_sequence),
                 retained_events = EXCLUDED.retained_events,
                 retained_bytes = EXCLUDED.retained_bytes,
                 journal_complete = state.journal_complete AND
                     EXCLUDED.retained_events =
                         EXCLUDED.latest_sequence - state.dropped_through AND
                     (SELECT MIN(retained.sequence) = state.dropped_through + 1
                      FROM {events} AS retained
                      WHERE retained.run_id = state.run_id),
                 terminal_event_sequence = COALESCE(
                     state.terminal_event_sequence, EXCLUDED.terminal_event_sequence),
                 updated_at = clock_timestamp();
             UPDATE {event_usage}
             SET retained_events = (SELECT COUNT(*)::BIGINT FROM {events}),
                 retained_bytes = (SELECT COALESCE(SUM(accounted_bytes), 0)::BIGINT FROM {events}),
                 updated_at = clock_timestamp()
             WHERE singleton = TRUE"
        );
        let stored_run_event_schema_version: i32 =
            sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                "SELECT schema_version FROM {event_usage} \
                 WHERE singleton = TRUE FOR UPDATE"
            )))
            .fetch_one(&mut *tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "Failed to read PostgreSQL run-event schema version: {error}"
                ))
            })?;
        if stored_run_event_schema_version > RUN_EVENT_SCHEMA_VERSION {
            return Err(IronCrewError::Validation(format!(
                "PostgreSQL run-event schema version {stored_run_event_schema_version} is newer than supported version {RUN_EVENT_SCHEMA_VERSION}"
            )));
        }
        if stored_run_event_schema_version < RUN_EVENT_SCHEMA_VERSION {
            sqlx::raw_sql(sqlx::AssertSqlSafe(reconcile_run_events))
                .execute(&mut *tx)
                .await
                .map_err(|error| {
                    IronCrewError::Validation(format!(
                        "Failed to reconcile PostgreSQL run-event journal accounting: {error}"
                    ))
                })?;
            let mark_version_sql = format!(
                "UPDATE {event_usage} SET schema_version = $1, \
                     updated_at = clock_timestamp() WHERE singleton = TRUE"
            );
            sqlx::query(sqlx::AssertSqlSafe(mark_version_sql))
                .bind(RUN_EVENT_SCHEMA_VERSION)
                .execute(&mut *tx)
                .await
                .map_err(|error| {
                    IronCrewError::Validation(format!(
                        "Failed to mark PostgreSQL run-event schema version: {error}"
                    ))
                })?;
        }

        for (column, data_type) in [
            ("owner_instance_id", "text"),
            ("lease_expires_at", "text"),
            ("task_results", "jsonb"),
            ("tags", "jsonb"),
        ] {
            let valid: bool = sqlx::query_scalar(
                "SELECT EXISTS (\
                    SELECT 1 FROM information_schema.columns \
                    WHERE table_schema = current_schema() \
                      AND table_name = $1 AND column_name = $2 AND data_type = $3\
                )",
            )
            .bind(t)
            .bind(column)
            .bind(data_type)
            .fetch_one(&mut *tx)
            .await
            .map_err(|e| {
                IronCrewError::Validation(format!(
                    "Failed to verify required run column '{column}': {e}"
                ))
            })?;
            if !valid {
                return Err(IronCrewError::Validation(format!(
                    "PostgreSQL column '{t}.{column}' is missing or is not {data_type}"
                )));
            }
        }

        tx.commit().await.map_err(|e| {
            IronCrewError::Validation(format!("Failed to commit PostgreSQL bootstrap: {e}"))
        })?;
        self.verify_required_schema().await?;

        tracing::debug!(
            "PostgreSQL bootstrap complete for tables '{}', '{}', '{}', '{}', '{}', '{}', '{}', '{}', '{}', '{}'",
            self.table_name,
            self.conversations_table,
            self.dialogs_table,
            self.audit_events_table,
            self.idempotency_table,
            self.idempotency_accounting_table,
            self.human_inputs_table,
            self.run_events_table,
            self.run_event_state_table,
            self.run_event_usage_table
        );
        Ok(())
    }

    /// Verify invariants required for safe multi-instance operation. Readiness
    /// uses the same check, so a manually altered schema cannot remain ready.
    async fn verify_required_schema(&self) -> Result<()> {
        let required_columns: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM information_schema.columns \
             WHERE table_schema = current_schema() AND table_name = $1 \
               AND (column_name, data_type) IN (\
                   ('owner_instance_id', 'text'), \
                   ('lease_expires_at', 'text'), \
                   ('task_results', 'jsonb'), \
                   ('tags', 'jsonb')\
               )",
        )
        .bind(&self.table_name)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| {
            IronCrewError::Validation(format!("Failed to verify PostgreSQL run schema: {e}"))
        })?;
        if required_columns != 4 {
            return Err(IronCrewError::Validation(format!(
                "PostgreSQL schema for '{}' is missing one or more required typed columns",
                self.table_name
            )));
        }

        let conversation_index = format!("uniq_{}_flow_id", self.conversations_table);
        let dialog_index = format!("uniq_{}_flow_id", self.dialogs_table);
        let valid_indexes: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) \
             FROM pg_index i \
             JOIN pg_class idx ON idx.oid = i.indexrelid \
             JOIN pg_class tbl ON tbl.oid = i.indrelid \
             JOIN pg_namespace ns ON ns.oid = tbl.relnamespace \
             WHERE ns.nspname = current_schema() \
               AND ((tbl.relname = $1 AND idx.relname = $2) \
                 OR (tbl.relname = $3 AND idx.relname = $4)) \
               AND i.indisunique AND i.indnullsnotdistinct \
               AND i.indnkeyatts = 2 \
               AND pg_get_indexdef(i.indexrelid, 1, TRUE) = 'flow_path' \
               AND pg_get_indexdef(i.indexrelid, 2, TRUE) = 'id'",
        )
        .bind(&self.conversations_table)
        .bind(&conversation_index)
        .bind(&self.dialogs_table)
        .bind(&dialog_index)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| {
            IronCrewError::Validation(format!("Failed to verify PostgreSQL session schema: {e}"))
        })?;
        if valid_indexes != 2 {
            return Err(IronCrewError::Validation(
                "PostgreSQL schema is missing a required UNIQUE NULLS NOT DISTINCT (flow_path, id) session index"
                    .into(),
            ));
        }
        let revision_columns: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM information_schema.columns \
             WHERE table_schema = current_schema() \
               AND ((table_name = $1 AND column_name = 'revision' AND data_type = 'bigint') \
                 OR (table_name = $2 AND column_name = 'revision' AND data_type = 'bigint'))",
        )
        .bind(&self.conversations_table)
        .bind(&self.dialogs_table)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| {
            IronCrewError::Validation(format!(
                "Failed to verify PostgreSQL session revisions: {e}"
            ))
        })?;
        if revision_columns != 2 {
            return Err(IronCrewError::Validation(
                "PostgreSQL session tables are missing required BIGINT revision columns".into(),
            ));
        }

        let conversation_execution_column: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM information_schema.columns \
             WHERE table_schema = current_schema() AND table_name = $1 \
               AND column_name = 'execution' AND data_type = 'jsonb' \
               AND is_nullable = 'NO'",
        )
        .bind(&self.conversations_table)
        .fetch_one(&self.pool)
        .await
        .map_err(|error| {
            IronCrewError::Validation(format!(
                "Failed to verify PostgreSQL conversation execution identity: {error}"
            ))
        })?;
        if conversation_execution_column != 1 {
            return Err(IronCrewError::Validation(
                "PostgreSQL conversation table is missing the required non-null JSONB execution identity column"
                    .into(),
            ));
        }

        let idempotency_columns: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) \
             FROM information_schema.columns AS c \
             JOIN (VALUES \
                 ('key_hash', 'text'), \
                 ('principal_id', 'text'), \
                 ('request_fingerprint', 'text'), \
                 ('operation', 'text'), \
                 ('scope', 'text'), \
                 ('resource_id', 'text'), \
                 ('exclusive_scope', 'text'), \
                 ('attempt_id', 'text'), \
                 ('owner_instance_id', 'text'), \
                 ('base_revision', 'bigint'), \
                 ('state', 'text'), \
                 ('response_status', 'integer'), \
                 ('response_body', 'text'), \
                 ('lease_expires_at', 'text'), \
                 ('created_at', 'text'), \
                 ('updated_at', 'text'), \
                 ('completed_at', 'text'), \
                 ('expires_at', 'text'), \
                 ('cancel_requested_at', 'text'), \
                 ('owner_draining_at', 'text'), \
                 ('ttl_seconds', 'bigint') \
             ) AS required(column_name, data_type) \
               ON required.column_name = c.column_name \
              AND required.data_type = c.data_type \
             WHERE c.table_schema = current_schema() AND c.table_name = $1",
        )
        .bind(&self.idempotency_table)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| {
            IronCrewError::Validation(format!(
                "Failed to verify PostgreSQL idempotency columns: {e}"
            ))
        })?;
        if idempotency_columns != 21 {
            return Err(IronCrewError::Validation(format!(
                "PostgreSQL schema for '{}' is missing one or more required typed columns",
                self.idempotency_table
            )));
        }

        let idempotency_primary_key: bool = sqlx::query_scalar(
            "SELECT EXISTS (\
                 SELECT 1 \
                 FROM pg_constraint con \
                 JOIN pg_class tbl ON tbl.oid = con.conrelid \
                 JOIN pg_namespace ns ON ns.oid = tbl.relnamespace \
                 JOIN pg_attribute attr \
                   ON attr.attrelid = tbl.oid AND attr.attnum = con.conkey[1] \
                 WHERE ns.nspname = current_schema() AND tbl.relname = $1 \
                   AND con.contype = 'p' AND cardinality(con.conkey) = 1 \
                   AND attr.attname = 'key_hash'\
             )",
        )
        .bind(&self.idempotency_table)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| {
            IronCrewError::Validation(format!(
                "Failed to verify PostgreSQL idempotency primary key: {e}"
            ))
        })?;
        if !idempotency_primary_key {
            return Err(IronCrewError::Validation(format!(
                "PostgreSQL table '{}' must have key_hash as its primary key",
                self.idempotency_table
            )));
        }

        let expires_index = format!("{}_exp_idx", self.idempotency_table);
        let resource_index = format!("{}_res_idx", self.idempotency_table);
        let lease_index = format!("{}_lease_idx", self.idempotency_table);
        let scope_index = format!("{}_scope_uidx", self.idempotency_table);
        let owner_index = format!("{}_owner_idx", self.idempotency_table);
        let valid_idempotency_indexes: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) \
             FROM pg_index i \
             JOIN pg_class idx ON idx.oid = i.indexrelid \
             JOIN pg_class tbl ON tbl.oid = i.indrelid \
             JOIN pg_namespace ns ON ns.oid = tbl.relnamespace \
             WHERE ns.nspname = current_schema() AND tbl.relname = $1 \
               AND (\
                 (idx.relname = $2 AND i.indnkeyatts = 1 \
                   AND pg_get_indexdef(i.indexrelid, 1, TRUE) = 'expires_at') \
                 OR \
                 (idx.relname = $3 AND i.indnkeyatts = 3 \
                   AND pg_get_indexdef(i.indexrelid, 1, TRUE) = 'operation' \
                   AND pg_get_indexdef(i.indexrelid, 2, TRUE) = 'scope' \
                   AND pg_get_indexdef(i.indexrelid, 3, TRUE) = 'resource_id') \
                 OR \
                 (idx.relname = $4 AND i.indnkeyatts = 3 \
                   AND pg_get_indexdef(i.indexrelid, 1, TRUE) = 'operation' \
                   AND pg_get_indexdef(i.indexrelid, 2, TRUE) = 'lease_expires_at' \
                   AND pg_get_indexdef(i.indexrelid, 3, TRUE) = 'key_hash' \
                   AND i.indpred IS NOT NULL \
                   AND pg_get_expr(i.indpred, i.indrelid) LIKE '%claimed%' \
                   AND pg_get_expr(i.indpred, i.indrelid) LIKE '%running%') \
                 OR \
                 (idx.relname = $5 AND i.indisunique AND i.indnkeyatts = 1 \
                   AND pg_get_indexdef(i.indexrelid, 1, TRUE) = 'exclusive_scope' \
                   AND i.indpred IS NOT NULL \
                   AND pg_get_expr(i.indpred, i.indrelid) LIKE '%exclusive_scope IS NOT NULL%' \
                   AND pg_get_expr(i.indpred, i.indrelid) LIKE '%claimed%' \
                   AND pg_get_expr(i.indpred, i.indrelid) LIKE '%running%') \
                 OR \
                 (idx.relname = $6 AND i.indnkeyatts = 2 \
                   AND pg_get_indexdef(i.indexrelid, 1, TRUE) = 'owner_instance_id' \
                   AND pg_get_indexdef(i.indexrelid, 2, TRUE) = 'operation' \
                   AND i.indpred IS NOT NULL \
                   AND pg_get_expr(i.indpred, i.indrelid) LIKE '%claimed%' \
                   AND pg_get_expr(i.indpred, i.indrelid) LIKE '%running%')\
               )",
        )
        .bind(&self.idempotency_table)
        .bind(&expires_index)
        .bind(&resource_index)
        .bind(&lease_index)
        .bind(&scope_index)
        .bind(&owner_index)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| {
            IronCrewError::Validation(format!(
                "Failed to verify PostgreSQL idempotency indexes: {e}"
            ))
        })?;
        if valid_idempotency_indexes != 5 {
            return Err(IronCrewError::Validation(format!(
                "PostgreSQL table '{}' is missing one or more required idempotency indexes",
                self.idempotency_table
            )));
        }

        let human_input_columns: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM information_schema.columns AS c \
             JOIN (VALUES \
                 ('run_id', 'text'), \
                 ('question_id', 'text'), \
                 ('flow', 'text'), \
                 ('owner_instance_id', 'text'), \
                 ('key_hash', 'text'), \
                 ('attempt_id', 'text'), \
                 ('question_digest', 'text'), \
                 ('question_key_fingerprint', 'text'), \
                 ('question_nonce', 'bytea'), \
                 ('question_ciphertext', 'bytea'), \
                 ('answer_key_fingerprint', 'text'), \
                 ('answer_nonce', 'bytea'), \
                 ('answer_ciphertext', 'bytea'), \
                 ('state', 'text'), \
                 ('created_at', 'timestamp with time zone'), \
                 ('expires_at', 'timestamp with time zone'), \
                 ('answered_at', 'timestamp with time zone') \
             ) AS required(column_name, data_type) \
               ON required.column_name = c.column_name \
              AND required.data_type = c.data_type \
             WHERE c.table_schema = current_schema() AND c.table_name = $1",
        )
        .bind(&self.human_inputs_table)
        .fetch_one(&self.pool)
        .await
        .map_err(|error| {
            IronCrewError::Validation(format!(
                "Failed to verify PostgreSQL human-input mailbox columns: {error}"
            ))
        })?;
        if human_input_columns != 17 {
            return Err(IronCrewError::Validation(format!(
                "PostgreSQL schema for '{}' is missing one or more human-input mailbox columns",
                self.human_inputs_table
            )));
        }

        let human_input_constraints: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM pg_constraint AS con \
             JOIN pg_class AS tbl ON tbl.oid = con.conrelid \
             JOIN pg_namespace AS ns ON ns.oid = tbl.relnamespace \
             LEFT JOIN pg_class AS referenced ON referenced.oid = con.confrelid \
             LEFT JOIN pg_namespace AS referenced_ns ON referenced_ns.oid = referenced.relnamespace \
             WHERE ns.nspname = current_schema() AND tbl.relname = $1 AND (\
                 (con.contype = 'p' AND cardinality(con.conkey) = 2 AND \
                  (SELECT array_agg(attr.attname ORDER BY key.ordinality) \
                   FROM unnest(con.conkey) WITH ORDINALITY AS key(attnum, ordinality) \
                   JOIN pg_attribute AS attr ON attr.attrelid = tbl.oid \
                                             AND attr.attnum = key.attnum) \
                    = ARRAY['run_id', 'question_id']::name[]) OR \
                 (con.contype = 'f' AND con.confdeltype = 'c' AND \
                  cardinality(con.conkey) = 1 AND \
                  (SELECT attr.attname FROM pg_attribute AS attr \
                   WHERE attr.attrelid = tbl.oid AND attr.attnum = con.conkey[1]) = 'run_id' AND \
                  referenced_ns.nspname = current_schema() AND referenced.relname = $5 AND \
                  (SELECT attr.attname FROM pg_attribute AS attr \
                   WHERE attr.attrelid = referenced.oid AND attr.attnum = con.confkey[1]) = 'run_id') OR \
                 (con.contype = 'c' AND con.conname IN ($2, $3, $4))\
             )",
        )
        .bind(&self.human_inputs_table)
        .bind(format!("{}_state_ck", self.human_inputs_table))
        .bind(format!("{}_payload_ck", self.human_inputs_table))
        .bind(format!("{}_expiry_ck", self.human_inputs_table))
        .bind(&self.table_name)
        .fetch_one(&self.pool)
        .await
        .map_err(|error| {
            IronCrewError::Validation(format!(
                "Failed to verify PostgreSQL human-input mailbox constraints: {error}"
            ))
        })?;
        if human_input_constraints != 5 {
            return Err(IronCrewError::Validation(format!(
                "PostgreSQL table '{}' is missing a required primary key, cascading run foreign key, or state/payload/expiry check",
                self.human_inputs_table
            )));
        }

        let human_run_index = format!("{}_run_idx", self.human_inputs_table);
        let human_expiry_index = format!("{}_exp_idx", self.human_inputs_table);
        let human_pending_expiry_index = format!("{}_pex_idx", self.human_inputs_table);
        let human_input_indexes: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM pg_index AS i \
             JOIN pg_class AS idx ON idx.oid = i.indexrelid \
             JOIN pg_class AS tbl ON tbl.oid = i.indrelid \
             JOIN pg_namespace AS ns ON ns.oid = tbl.relnamespace \
             WHERE ns.nspname = current_schema() AND tbl.relname = $1 AND (\
                 (idx.relname = $2 AND i.indnkeyatts = 2 AND \
                  pg_get_indexdef(i.indexrelid, 1, TRUE) = 'run_id' AND \
                  pg_get_indexdef(i.indexrelid, 2, TRUE) = 'expires_at' AND \
                  i.indpred IS NOT NULL AND \
                  pg_get_expr(i.indpred, i.indrelid) LIKE '%pending%') OR \
                 (idx.relname = $3 AND i.indnkeyatts = 1 AND \
                  pg_get_indexdef(i.indexrelid, 1, TRUE) = 'expires_at') OR \
                 (idx.relname = $4 AND i.indnkeyatts = 3 AND \
                  pg_get_indexdef(i.indexrelid, 1, TRUE) = 'expires_at' AND \
                  pg_get_indexdef(i.indexrelid, 2, TRUE) = 'run_id' AND \
                  pg_get_indexdef(i.indexrelid, 3, TRUE) = 'question_id' AND \
                  i.indpred IS NOT NULL AND \
                  pg_get_expr(i.indpred, i.indrelid) LIKE '%pending%')\
             )",
        )
        .bind(&self.human_inputs_table)
        .bind(&human_run_index)
        .bind(&human_expiry_index)
        .bind(&human_pending_expiry_index)
        .fetch_one(&self.pool)
        .await
        .map_err(|error| {
            IronCrewError::Validation(format!(
                "Failed to verify PostgreSQL human-input mailbox indexes: {error}"
            ))
        })?;
        if human_input_indexes != 3 {
            return Err(IronCrewError::Validation(format!(
                "PostgreSQL table '{}' is missing one or more required human-input mailbox indexes",
                self.human_inputs_table
            )));
        }

        let accounting_columns: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM information_schema.columns AS c \
             JOIN (VALUES \
                 ('principal_id', 'text'), \
                 ('is_global', 'boolean'), \
                 ('record_count', 'bigint'), \
                 ('in_flight_count', 'bigint'), \
                 ('response_bytes', 'bigint'), \
                 ('updated_at', 'timestamp with time zone') \
             ) AS required(column_name, data_type) \
               ON required.column_name = c.column_name \
              AND required.data_type = c.data_type \
             WHERE c.table_schema = current_schema() AND c.table_name = $1",
        )
        .bind(&self.idempotency_accounting_table)
        .fetch_one(&self.pool)
        .await
        .map_err(|error| {
            IronCrewError::Validation(format!(
                "Failed to verify PostgreSQL idempotency accounting columns: {error}"
            ))
        })?;
        if accounting_columns != 6 {
            return Err(IronCrewError::Validation(format!(
                "PostgreSQL schema for '{}' is missing one or more accounting columns",
                self.idempotency_accounting_table
            )));
        }
        let accounting_trigger = format!("{}_acct_trg", self.idempotency_table);
        let trigger_exists: bool = sqlx::query_scalar(
            "SELECT EXISTS (\
                 SELECT 1 FROM pg_trigger AS trg \
                 JOIN pg_class AS tbl ON tbl.oid = trg.tgrelid \
                 JOIN pg_namespace AS ns ON ns.oid = tbl.relnamespace \
                 WHERE ns.nspname = current_schema() AND tbl.relname = $1 \
                   AND trg.tgname = $2 AND NOT trg.tgisinternal \
                   AND trg.tgenabled <> 'D'\
             )",
        )
        .bind(&self.idempotency_table)
        .bind(&accounting_trigger)
        .fetch_one(&self.pool)
        .await
        .map_err(|error| {
            IronCrewError::Validation(format!(
                "Failed to verify PostgreSQL idempotency accounting trigger: {error}"
            ))
        })?;
        if !trigger_exists {
            return Err(IronCrewError::Validation(
                "PostgreSQL idempotency accounting trigger is missing or disabled".into(),
            ));
        }
        let global_accounting_valid: bool = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT EXISTS (SELECT 1 FROM {} \
                 WHERE principal_id = 'global' AND is_global = TRUE \
                   AND record_count >= 0 AND in_flight_count >= 0 AND response_bytes >= 0)",
            self.idempotency_accounting_table
        )))
        .fetch_one(&self.pool)
        .await
        .map_err(|error| {
            IronCrewError::Validation(format!(
                "Failed to verify PostgreSQL global idempotency accounting: {error}"
            ))
        })?;
        if !global_accounting_valid {
            return Err(IronCrewError::Validation(
                "PostgreSQL global idempotency accounting row is missing or invalid".into(),
            ));
        }

        let run_event_columns: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM information_schema.columns AS column_info \
             JOIN (VALUES \
                 ($1::text, 'run_id', 'text'), \
                 ($1::text, 'sequence', 'bigint'), \
                 ($1::text, 'event_type', 'text'), \
                 ($1::text, 'payload', 'jsonb'), \
                 ($1::text, 'payload_bytes', 'bigint'), \
                 ($1::text, 'accounted_bytes', 'bigint'), \
                 ($1::text, 'created_at', 'timestamp with time zone'), \
                 ($1::text, 'expires_at', 'timestamp with time zone'), \
                 ($2::text, 'run_id', 'text'), \
                 ($2::text, 'flow', 'text'), \
                 ($2::text, 'owner_instance_id', 'text'), \
                 ($2::text, 'latest_sequence', 'bigint'), \
                 ($2::text, 'dropped_through', 'bigint'), \
                 ($2::text, 'retained_events', 'bigint'), \
                 ($2::text, 'retained_bytes', 'bigint'), \
                 ($2::text, 'journal_complete', 'boolean'), \
                 ($2::text, 'eviction_reason', 'text'), \
                 ($2::text, 'terminal_event_sequence', 'bigint'), \
                 ($2::text, 'updated_at', 'timestamp with time zone'), \
                 ($3::text, 'singleton', 'boolean'), \
                 ($3::text, 'schema_version', 'integer'), \
                 ($3::text, 'retained_events', 'bigint'), \
                 ($3::text, 'retained_bytes', 'bigint'), \
                 ($3::text, 'updated_at', 'timestamp with time zone') \
             ) AS required(table_name, column_name, data_type) \
               ON required.table_name = column_info.table_name \
              AND required.column_name = column_info.column_name \
              AND required.data_type = column_info.data_type \
             WHERE column_info.table_schema = current_schema()",
        )
        .bind(&self.run_events_table)
        .bind(&self.run_event_state_table)
        .bind(&self.run_event_usage_table)
        .fetch_one(&self.pool)
        .await
        .map_err(|error| {
            IronCrewError::Validation(format!(
                "Failed to verify PostgreSQL run-event journal columns: {error}"
            ))
        })?;
        if run_event_columns != 24 {
            return Err(IronCrewError::Validation(
                "PostgreSQL run-event journal is missing one or more required typed columns".into(),
            ));
        }

        let run_event_constraints: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM pg_constraint AS con \
             JOIN pg_class AS table_info ON table_info.oid = con.conrelid \
             JOIN pg_namespace AS namespace ON namespace.oid = table_info.relnamespace \
             LEFT JOIN pg_class AS referenced ON referenced.oid = con.confrelid \
             LEFT JOIN pg_namespace AS referenced_namespace \
                    ON referenced_namespace.oid = referenced.relnamespace \
             WHERE namespace.nspname = current_schema() AND (\
                 (table_info.relname = $1 AND con.contype = 'p' AND \
                  cardinality(con.conkey) = 2) OR \
                 (table_info.relname = $2 AND con.contype = 'p' AND \
                  cardinality(con.conkey) = 1) OR \
                 (table_info.relname = $3 AND con.contype = 'p' AND \
                  cardinality(con.conkey) = 1) OR \
                 (table_info.relname IN ($1, $2) AND con.contype = 'f' AND \
                  con.confdeltype = 'c' AND referenced_namespace.nspname = current_schema() AND \
                  referenced.relname = $4) OR \
                 (con.contype = 'c' AND con.conname IN ($5, $6, $7, $8, $9))\
             )",
        )
        .bind(&self.run_events_table)
        .bind(&self.run_event_state_table)
        .bind(&self.run_event_usage_table)
        .bind(&self.table_name)
        .bind(format!("{}_payload_ck", self.run_events_table))
        .bind(format!("{}_expiry_ck", self.run_events_table))
        .bind(format!("{}_bounds_ck", self.run_event_state_table))
        .bind(format!("{}_reason_ck", self.run_event_state_table))
        .bind(format!("{}_usage_ck", self.run_event_usage_table))
        .fetch_one(&self.pool)
        .await
        .map_err(|error| {
            IronCrewError::Validation(format!(
                "Failed to verify PostgreSQL run-event journal constraints: {error}"
            ))
        })?;
        if run_event_constraints != 10 {
            return Err(IronCrewError::Validation(
                "PostgreSQL run-event journal is missing a primary key, cascading run foreign key, or bounded-data constraint"
                    .into(),
            ));
        }

        let run_event_expiry_index = format!("{}_exp_idx", self.run_events_table);
        let run_event_oldest_index = format!("{}_old_idx", self.run_events_table);
        let run_event_indexes: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM pg_index AS index_info \
             JOIN pg_class AS index_class ON index_class.oid = index_info.indexrelid \
             JOIN pg_class AS table_info ON table_info.oid = index_info.indrelid \
             JOIN pg_namespace AS namespace ON namespace.oid = table_info.relnamespace \
             WHERE namespace.nspname = current_schema() AND table_info.relname = $1 AND (\
                 (index_class.relname = $2 AND index_info.indnkeyatts = 3 AND \
                  pg_get_indexdef(index_info.indexrelid, 1, TRUE) = 'expires_at' AND \
                  pg_get_indexdef(index_info.indexrelid, 2, TRUE) = 'run_id' AND \
                  pg_get_indexdef(index_info.indexrelid, 3, TRUE) = 'sequence') OR \
                 (index_class.relname = $3 AND index_info.indnkeyatts = 3 AND \
                  pg_get_indexdef(index_info.indexrelid, 1, TRUE) = 'created_at' AND \
                  pg_get_indexdef(index_info.indexrelid, 2, TRUE) = 'run_id' AND \
                  pg_get_indexdef(index_info.indexrelid, 3, TRUE) = 'sequence')\
             )",
        )
        .bind(&self.run_events_table)
        .bind(&run_event_expiry_index)
        .bind(&run_event_oldest_index)
        .fetch_one(&self.pool)
        .await
        .map_err(|error| {
            IronCrewError::Validation(format!(
                "Failed to verify PostgreSQL run-event journal indexes: {error}"
            ))
        })?;
        if run_event_indexes != 2 {
            return Err(IronCrewError::Validation(
                "PostgreSQL run-event journal is missing a retention or global-pruning index"
                    .into(),
            ));
        }

        let run_event_trigger = format!("{}_acct_trg", self.run_events_table);
        let run_event_trigger_exists: bool = sqlx::query_scalar(
            "SELECT EXISTS (\
                 SELECT 1 FROM pg_trigger AS trigger_info \
                 JOIN pg_class AS table_info ON table_info.oid = trigger_info.tgrelid \
                 JOIN pg_namespace AS namespace ON namespace.oid = table_info.relnamespace \
                 WHERE namespace.nspname = current_schema() AND table_info.relname = $1 \
                   AND trigger_info.tgname = $2 AND NOT trigger_info.tgisinternal \
                   AND trigger_info.tgenabled <> 'D'\
             )",
        )
        .bind(&self.run_events_table)
        .bind(&run_event_trigger)
        .fetch_one(&self.pool)
        .await
        .map_err(|error| {
            IronCrewError::Validation(format!(
                "Failed to verify PostgreSQL run-event accounting trigger: {error}"
            ))
        })?;
        if !run_event_trigger_exists {
            return Err(IronCrewError::Validation(
                "PostgreSQL run-event accounting trigger is missing or disabled".into(),
            ));
        }

        let run_event_usage_valid: bool = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT EXISTS (SELECT 1 FROM {} WHERE singleton = TRUE \
                     AND schema_version = {RUN_EVENT_SCHEMA_VERSION} \
                     AND retained_events >= 0 AND retained_bytes >= 0)",
            self.run_event_usage_table
        )))
        .fetch_one(&self.pool)
        .await
        .map_err(|error| {
            IronCrewError::Validation(format!(
                "Failed to verify PostgreSQL run-event global accounting: {error}"
            ))
        })?;
        if !run_event_usage_valid {
            return Err(IronCrewError::Validation(
                "PostgreSQL run-event global accounting row is missing or invalid".into(),
            ));
        }
        Ok(())
    }
}

#[async_trait]
impl StateStore for PostgresStore {
    async fn save_run_intent(&self, intent: RunIntent) -> Result<String> {
        self.insert_run_intent(intent).await
    }

    async fn update_run_completion(
        &self,
        run_id: &str,
        completion: RunCompletion,
    ) -> Result<RunTransition> {
        self.finish_run(run_id, completion).await
    }

    async fn update_run_status(
        &self,
        run_id: &str,
        status: crate::engine::run_history::RunStatus,
    ) -> Result<()> {
        self.set_run_status(run_id, status).await
    }

    fn instance_id(&self) -> &str {
        self.lease.instance_id()
    }

    fn postgres_pool_usage(&self) -> Option<crate::engine::store::PostgresPoolUsage> {
        let open_connections = self.pool.size();
        let idle_connections = u32::try_from(self.pool.num_idle())
            .unwrap_or(u32::MAX)
            .min(open_connections);
        Some(crate::engine::store::PostgresPoolUsage {
            open_connections,
            in_use_connections: open_connections.saturating_sub(idle_connections),
            connection_limit: self.pool.options().get_max_connections(),
        })
    }

    fn run_lease_ttl(&self) -> Duration {
        self.lease.ttl()
    }

    fn run_maintenance_watchdog(&self) -> Option<Duration> {
        Some(crate::engine::store::run_maintenance_timeout(
            self.lease.ttl(),
        ))
    }

    fn supports_durable_human_input(&self) -> bool {
        self.human_input_keyring.is_some()
    }

    fn event_journal_scope(&self) -> EventJournalScope {
        EventJournalScope::SharedStore
    }

    fn conversation_coordination_scope(&self) -> ConversationCoordinationScope {
        ConversationCoordinationScope::SharedStore
    }

    fn event_journal_config(&self) -> RunEventJournalConfig {
        self.run_event_journal_config.clone()
    }

    async fn append_run_events(
        &self,
        batch: &RunEventAppendBatch,
    ) -> Result<RunEventAppendOutcome> {
        self.append_run_event_journal(batch).await
    }
    async fn read_run_events(
        &self,
        flow: &str,
        run_id: &str,
        after_sequence: u64,
    ) -> Result<RunEventPage> {
        self.read_run_events_journal(flow, run_id, after_sequence)
            .await
    }
    async fn heartbeat_owned_runs(&self) -> Result<usize> {
        let mut tx = self.pool.begin().await.map_err(|error| {
            IronCrewError::Validation(format!("PG heartbeat transaction: {error}"))
        })?;
        self.configure_run_lease_transaction(&mut tx).await?;
        self.lock_run_fence(&mut tx, true).await?;
        let (_, deadline) = self
            .database_clock_with_deadline(
                &mut tx,
                self.lease.ttl().as_secs(),
                "run heartbeat lease",
            )
            .await?;
        let sql = format!(
            "UPDATE {runs} AS run SET lease_expires_at = $1
             WHERE run.owner_instance_id = $2
               AND run.status IN ('running', 'waiting_for_input')
               AND NOT EXISTS (
                   SELECT 1 FROM {idempotency} AS idem
                   WHERE idem.operation = $3 AND idem.resource_id = run.run_id
               )",
            runs = self.table_name,
            idempotency = self.idempotency_table
        );
        let result = sqlx::query(sqlx::AssertSqlSafe(sql.to_string()))
            .bind(&deadline)
            .bind(self.lease.instance_id())
            .bind(RUN_OPERATION)
            .execute(&mut *tx)
            .await
            .map_err(|e| IronCrewError::Validation(format!("PG heartbeat: {}", e)))?;
        tx.commit()
            .await
            .map_err(|error| IronCrewError::Validation(format!("PG heartbeat commit: {error}")))?;
        Ok(result.rows_affected() as usize)
    }

    async fn health_check(&self) -> Result<()> {
        self.verify_required_schema().await?;

        // Exercise the write privilege used by heartbeat/finalization without
        // mutating a row. A read-only credential must never report ready.
        let mut transaction = self.pool.begin().await.map_err(|e| {
            IronCrewError::Validation(format!("PostgreSQL health transaction: {e}"))
        })?;
        let sql = format!(
            "UPDATE {} SET lease_expires_at = lease_expires_at WHERE FALSE",
            self.table_name
        );
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .execute(&mut *transaction)
            .await
            .map_err(|e| {
                IronCrewError::Validation(format!("PostgreSQL health write probe: {e}"))
            })?;
        let idempotency_sql = format!(
            "UPDATE {} SET updated_at = updated_at WHERE FALSE",
            self.idempotency_table
        );
        sqlx::query(sqlx::AssertSqlSafe(idempotency_sql))
            .execute(&mut *transaction)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL idempotency health write probe: {error}"
                ))
            })?;
        let accounting_sql = format!(
            "UPDATE {} SET updated_at = updated_at WHERE FALSE",
            self.idempotency_accounting_table
        );
        sqlx::query(sqlx::AssertSqlSafe(accounting_sql))
            .execute(&mut *transaction)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL idempotency accounting health write probe: {error}"
                ))
            })?;
        let human_input_sql = format!(
            "UPDATE {} SET expires_at = expires_at WHERE FALSE",
            self.human_inputs_table
        );
        sqlx::query(sqlx::AssertSqlSafe(human_input_sql))
            .execute(&mut *transaction)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL human-input mailbox health write probe: {error}"
                ))
            })?;
        for (table, column) in [
            (&self.run_events_table, "created_at"),
            (&self.run_event_state_table, "updated_at"),
            (&self.run_event_usage_table, "updated_at"),
        ] {
            let sql = format!("UPDATE {table} SET {column} = {column} WHERE FALSE");
            sqlx::query(sqlx::AssertSqlSafe(sql))
                .execute(&mut *transaction)
                .await
                .map_err(|error| {
                    IronCrewError::Validation(format!(
                        "PostgreSQL run-event journal health write probe failed for '{table}': {error}"
                    ))
                })?;
        }
        transaction
            .rollback()
            .await
            .map_err(|e| IronCrewError::Validation(format!("PostgreSQL health rollback: {e}")))?;
        Ok(())
    }

    async fn reconcile_abandoned_runs(&self, now: &str) -> Result<usize> {
        parse_timestamp("reconciliation timestamp", now)?;
        let mut tx = self.pool.begin().await.map_err(|error| {
            IronCrewError::Validation(format!("PG reconcile transaction: {error}"))
        })?;
        self.configure_run_lease_transaction(&mut tx).await?;
        self.lock_idempotency_quota(&mut tx).await?;
        self.lock_run_fence(&mut tx, false).await?;
        let (database_now, _) = self
            .database_clock_with_deadline(&mut tx, 0, "run reconciliation")
            .await?;

        // A process may die after durably allocating/replying with a run id
        // but before publishing the normal run intent. Materialize those
        // tombstones and reconcile existing expired runs under one shared
        // fixed-size budget so a large history cannot repeatedly roll back
        // without making progress.
        let reserved = RUN_RECONCILIATION_BATCH_SIZE / 2;
        let mut fallback_ids = self
            .materialize_abandoned_claim_batch(&mut tx, &database_now, reserved)
            .await?;
        let mut expired_ids = self
            .reconcile_expired_run_batch(&mut tx, &database_now, reserved)
            .await?;
        let selected = i64::try_from(fallback_ids.len().saturating_add(expired_ids.len()))
            .map_err(|_| {
                IronCrewError::Validation("PostgreSQL reconciliation batch size overflow".into())
            })?;
        let remaining = RUN_RECONCILIATION_BATCH_SIZE.saturating_sub(selected);
        if remaining > 0 && fallback_ids.len() == reserved as usize {
            fallback_ids.extend(
                self.materialize_abandoned_claim_batch(&mut tx, &database_now, remaining)
                    .await?,
            );
        } else if remaining > 0 && expired_ids.len() == reserved as usize {
            expired_ids.extend(
                self.reconcile_expired_run_batch(&mut tx, &database_now, remaining)
                    .await?,
            );
        }
        let reconciled = fallback_ids.len().saturating_add(expired_ids.len());
        fallback_ids.append(&mut expired_ids);

        // Keep every dependent write within the same transaction, but scope
        // it to this batch. Independent conversation tombstones and mailbox
        // expiry/terminal repair each receive their own fixed-size budget.
        self.finalize_reconciled_runs(&mut tx, &database_now, &fallback_ids)
            .await?;
        self.reconcile_expired_conversation_batch(&mut tx, &database_now)
            .await?;
        self.delete_expired_human_input_batch(&mut tx, &database_now)
            .await?;
        tx.commit()
            .await
            .map_err(|error| IronCrewError::Validation(format!("PG reconcile commit: {error}")))?;
        // Keep journal cleanup outside the core reconciliation transaction so
        // usage-row contention or malformed journal data cannot undo critical
        // run/idempotency/HITL recovery.
        self.prune_expired_run_events_best_effort().await;
        Ok(reconciled)
    }

    async fn get_run(&self, run_id: &str) -> Result<RunRecord> {
        self.load_run(run_id).await
    }

    async fn list_runs_summary(
        &self,
        filter: &ListRunsFilter,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<RunSummary>> {
        self.load_run_summaries(filter, limit, offset).await
    }

    async fn count_runs(&self, filter: &ListRunsFilter) -> Result<u64> {
        self.load_run_count(filter).await
    }

    async fn delete_run(&self, run_id: &str) -> Result<()> {
        self.remove_run(run_id).await
    }

    async fn lookup_idempotency_for_principal(
        &self,
        principal_id: &PrincipalId,
        key_hash: &str,
        request_fingerprint: &str,
        now: &str,
    ) -> Result<IdempotencyLookup> {
        self.lookup_idempotency_for_principal_record(
            principal_id,
            key_hash,
            request_fingerprint,
            now,
        )
        .await
    }
    async fn claim_idempotency_with_limits(
        &self,
        claim: IdempotencyClaim,
        limits: IdempotencyLimits,
    ) -> Result<IdempotencyClaimOutcome> {
        self.claim_idempotency_record(claim, limits).await
    }
    async fn heartbeat_idempotency(
        &self,
        key_hash: &str,
        attempt_id: &str,
        new_lease_expires_at: &str,
    ) -> Result<bool> {
        self.heartbeat_idempotency_record(key_hash, attempt_id, new_lease_expires_at)
            .await
    }
    async fn heartbeat_idempotent_run(
        &self,
        run_id: &str,
        key_hash: &str,
        attempt_id: &str,
        new_lease_expires_at: &str,
    ) -> Result<RunFenceHeartbeat> {
        self.heartbeat_idempotent_run_record(run_id, key_hash, attempt_id, new_lease_expires_at)
            .await
    }
    async fn begin_owner_drain(&self) -> Result<usize> {
        let mut tx = self.pool.begin().await.map_err(|error| {
            IronCrewError::Validation(format!(
                "PostgreSQL owner-drain transaction failed: {error}"
            ))
        })?;
        self.configure_run_lease_transaction(&mut tx).await?;
        // This one-time exclusive fence serializes with run claim, intent,
        // heartbeat, cancellation, HITL, and terminalization transactions.
        self.lock_run_fence(&mut tx, false).await?;
        let (database_now, _) = self
            .database_clock_with_deadline(&mut tx, 0, "owner drain")
            .await?;
        let sql = format!(
            "UPDATE {} SET \
                 owner_draining_at = COALESCE(owner_draining_at, $1), \
                 updated_at = CASE WHEN owner_draining_at IS NULL THEN $1 ELSE updated_at END \
             WHERE operation = $2 AND owner_instance_id = $3 \
               AND state IN ('claimed', 'running')",
            self.idempotency_table
        );
        let result = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(&database_now)
            .bind(RUN_OPERATION)
            .bind(self.lease.instance_id())
            .execute(&mut *tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL owner-drain fence update failed: {error}"
                ))
            })?;
        tx.commit().await.map_err(|error| {
            IronCrewError::Validation(format!(
                "PostgreSQL owner-drain fence commit failed: {error}"
            ))
        })?;
        usize::try_from(result.rows_affected()).map_err(|_| {
            IronCrewError::Validation(
                "PostgreSQL owner-drain fence count exceeded process limits".into(),
            )
        })
    }

    async fn request_run_cancellation(
        &self,
        run_id: &str,
        flow: &str,
    ) -> Result<RunCancellationRequest> {
        if run_id.is_empty() || run_id.len() > 128 {
            return Err(IronCrewError::Validation(
                "Cancellation run id must be 1..=128 bytes".into(),
            ));
        }
        if flow.is_empty() || flow.len() > 255 || flow.chars().any(char::is_control) {
            return Err(IronCrewError::Validation(
                "Cancellation flow must be 1..=255 printable bytes".into(),
            ));
        }

        let mut tx = self.pool.begin().await.map_err(|error| {
            IronCrewError::Validation(format!(
                "PostgreSQL run cancellation transaction failed: {error}"
            ))
        })?;
        // Match the run-intent/heartbeat lock order. This serializes the
        // cancellation request with terminalization without blocking other
        // unrelated runs.
        self.lock_run_fence(&mut tx, true).await?;
        self.lock_resource(&mut tx, RUN_OPERATION, "", run_id)
            .await?;

        let run_sql = format!(
            "SELECT status, owner_instance_id, flow FROM {} WHERE run_id = $1 FOR UPDATE",
            self.table_name
        );
        let Some(run) = sqlx::query(sqlx::AssertSqlSafe(run_sql))
            .bind(run_id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL cancellation run lookup failed: {error}"
                ))
            })?
        else {
            tx.commit().await.map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL missing cancellation run commit failed: {error}"
                ))
            })?;
            return Ok(RunCancellationRequest::NotFound);
        };
        let run_flow: String = run
            .try_get("flow")
            .map_err(|error| IronCrewError::Validation(format!("Column error: {error}")))?;
        if run_flow != flow {
            tx.commit().await.map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL scoped cancellation lookup commit failed: {error}"
                ))
            })?;
            return Ok(RunCancellationRequest::NotFound);
        }
        let status = run
            .try_get::<String, _>("status")
            .map_err(|error| IronCrewError::Validation(format!("Column error: {error}")))?
            .parse::<RunStatus>()?;
        if status.is_terminal() {
            self.delete_human_inputs_for_run(&mut tx, run_id).await?;
            tx.commit().await.map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL terminal cancellation lookup commit failed: {error}"
                ))
            })?;
            return Ok(RunCancellationRequest::Terminal(status));
        }
        let run_owner: String = run
            .try_get("owner_instance_id")
            .map_err(|error| IronCrewError::Validation(format!("Column error: {error}")))?;

        let key_sql = format!(
            "SELECT key_hash FROM {} \
             WHERE operation = $1 AND scope = $2 AND resource_id = $3 \
               AND state IN ('claimed', 'running') \
             ORDER BY created_at DESC LIMIT 2",
            self.idempotency_table
        );
        let keys: Vec<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(key_sql))
            .bind(RUN_OPERATION)
            .bind(flow)
            .bind(run_id)
            .fetch_all(&mut *tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL cancellation ledger lookup failed: {error}"
                ))
            })?;
        let [key_hash] = keys.as_slice() else {
            if keys.len() > 1 {
                return Err(IronCrewError::Conflict(format!(
                    "Run '{run_id}' has multiple active idempotency ledgers"
                )));
            }
            tx.commit().await.map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL non-durable cancellation lookup commit failed: {error}"
                ))
            })?;
            return Ok(RunCancellationRequest::NotDurable);
        };

        self.lock_idempotency_key(&mut tx, key_hash).await?;
        let ledger_sql = format!(
            "SELECT owner_instance_id, state, cancel_requested_at, owner_draining_at FROM {} \
             WHERE key_hash = $1 FOR UPDATE",
            self.idempotency_table
        );
        let Some(ledger) = sqlx::query(sqlx::AssertSqlSafe(ledger_sql))
            .bind(key_hash)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL cancellation ledger fence failed: {error}"
                ))
            })?
        else {
            tx.rollback().await.map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL vanished cancellation ledger rollback failed: {error}"
                ))
            })?;
            return Ok(RunCancellationRequest::NotDurable);
        };
        let ledger_owner: String = ledger
            .try_get("owner_instance_id")
            .map_err(|error| IronCrewError::Validation(format!("Column error: {error}")))?;
        let ledger_state = ledger
            .try_get::<String, _>("state")
            .map_err(|error| IronCrewError::Validation(format!("Column error: {error}")))?
            .parse::<IdempotencyState>()?;
        if ledger_owner != run_owner || !ledger_state.is_in_flight() {
            tx.rollback().await.map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL changed cancellation fence rollback failed: {error}"
                ))
            })?;
            return Ok(RunCancellationRequest::NotDurable);
        }
        let owner_draining = ledger
            .try_get::<Option<String>, _>("owner_draining_at")
            .map_err(|error| IronCrewError::Validation(format!("Column error: {error}")))?
            .is_some();
        if owner_draining {
            tx.commit().await.map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL draining-owner cancellation commit failed: {error}"
                ))
            })?;
            return Ok(RunCancellationRequest::OwnerDraining {
                owner_instance_id: run_owner,
            });
        }
        let already_requested = ledger
            .try_get::<Option<String>, _>("cancel_requested_at")
            .map_err(|error| IronCrewError::Validation(format!("Column error: {error}")))?
            .is_some();
        let (database_now, _) = self
            .database_clock_with_deadline(&mut tx, 0, "run cancellation request")
            .await?;
        let update_sql = format!(
            "UPDATE {} SET \
                 cancel_requested_at = COALESCE(cancel_requested_at, $1), \
                 updated_at = CASE WHEN cancel_requested_at IS NULL THEN $1 ELSE updated_at END \
             WHERE key_hash = $2 AND owner_instance_id = $3 \
               AND state IN ('claimed', 'running')",
            self.idempotency_table
        );
        let changed = sqlx::query(sqlx::AssertSqlSafe(update_sql))
            .bind(&database_now)
            .bind(key_hash)
            .bind(&run_owner)
            .execute(&mut *tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL cancellation request failed: {error}"
                ))
            })?;
        if changed.rows_affected() != 1 {
            tx.rollback().await.map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL cancellation race rollback failed: {error}"
                ))
            })?;
            return Ok(RunCancellationRequest::NotDurable);
        }
        self.delete_human_inputs_for_run(&mut tx, run_id).await?;
        tx.commit().await.map_err(|error| {
            IronCrewError::Validation(format!(
                "PostgreSQL cancellation request commit failed: {error}"
            ))
        })?;
        Ok(RunCancellationRequest::Requested {
            owner_instance_id: run_owner,
            already_requested,
        })
    }

    async fn register_human_input(
        &self,
        registration: &DurableHumanInputRegistration,
    ) -> Result<HumanInputRegistrationOutcome> {
        self.register_human_input_record(registration).await
    }

    async fn list_human_inputs(&self, flow: &str, run_id: &str) -> Result<HumanInputListOutcome> {
        self.list_human_input_records(flow, run_id).await
    }

    async fn answer_human_input(
        &self,
        flow: &str,
        run_id: &str,
        question_id: &str,
        answer: &serde_json::Value,
    ) -> Result<HumanInputAnswerOutcome> {
        self.answer_human_input_record(flow, run_id, question_id, answer)
            .await
    }

    async fn read_human_input(
        &self,
        registration: &DurableHumanInputRegistration,
    ) -> Result<HumanInputReadOutcome> {
        self.read_human_input_record(registration).await
    }

    async fn close_human_input(
        &self,
        registration: &DurableHumanInputRegistration,
    ) -> Result<bool> {
        self.close_human_input_record(registration).await
    }

    async fn complete_idempotency_with_limits(
        &self,
        completion: IdempotencyCompletion,
        limits: IdempotencyLimits,
    ) -> Result<IdempotencyCompletionOutcome> {
        self.complete_idempotency_record(completion, limits).await
    }
    async fn commit_conversation_idempotency_with_limits(
        &self,
        completion: IdempotencyCompletion,
        conversation: &ConversationRecord,
        limits: IdempotencyLimits,
    ) -> Result<ConversationIdempotencyCommit> {
        self.commit_conversation_idempotency_record(completion, conversation, limits)
            .await
    }
    async fn mark_idempotency_indeterminate(
        &self,
        key_hash: &str,
        attempt_id: &str,
        completed_at: &str,
        expires_at: &str,
    ) -> Result<bool> {
        self.mark_idempotency_indeterminate_record(key_hash, attempt_id, completed_at, expires_at)
            .await
    }

    async fn release_idempotency(&self, key_hash: &str, attempt_id: &str) -> Result<bool> {
        self.release_idempotency_record(key_hash, attempt_id).await
    }

    async fn prune_idempotency(&self, now: &str, limit: usize) -> Result<usize> {
        self.prune_idempotency_records(now, limit).await
    }
    async fn idempotency_usage(
        &self,
        principal_id: &PrincipalId,
        limits: IdempotencyLimits,
    ) -> Result<IdempotencyUsage> {
        self.idempotency_usage_record(principal_id, limits).await
    }
    async fn save_conversation(&self, record: &ConversationRecord) -> Result<u64> {
        self.save_conversation_record(record).await
    }

    async fn get_conversation(
        &self,
        flow_path: Option<&str>,
        id: &str,
    ) -> Result<Option<ConversationRecord>> {
        self.get_conversation_record(flow_path, id).await
    }

    async fn delete_conversation(&self, flow_path: Option<&str>, id: &str) -> Result<()> {
        self.delete_conversation_record(flow_path, id).await
    }

    async fn list_conversations(
        &self,
        flow_path: Option<&str>,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<ConversationSummary>> {
        self.list_conversation_records(flow_path, limit, offset)
            .await
    }

    async fn count_conversations(&self, flow_path: Option<&str>) -> Result<u64> {
        self.count_conversation_records(flow_path).await
    }

    async fn save_dialog_state(&self, record: &DialogStateRecord) -> Result<u64> {
        self.save_dialog_state_record(record).await
    }

    async fn get_dialog_state(
        &self,
        flow_path: Option<&str>,
        id: &str,
    ) -> Result<Option<DialogStateRecord>> {
        self.get_dialog_state_record(flow_path, id).await
    }

    async fn delete_dialog_state(&self, flow_path: Option<&str>, id: &str) -> Result<()> {
        self.delete_dialog_state_record(flow_path, id).await
    }

    async fn save_audit_event(&self, event: &crate::engine::audit::AuditEvent) -> Result<String> {
        let id = uuid::Uuid::new_v4().to_string();
        let sql = format!(
            "INSERT INTO {at}
             (id, timestamp, action, flow_path, target, actor, source_ip, success, status_code, metadata)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10::jsonb)",
            at = self.audit_events_table
        );
        let metadata_str = match &event.metadata {
            Some(v) => Some(
                serde_json::to_string(v)
                    .map_err(|e| IronCrewError::Validation(format!("Metadata serialize: {}", e)))?,
            ),
            None => None,
        };
        sqlx::query(sqlx::AssertSqlSafe(sql.to_string()))
            .bind(&id)
            .bind(&event.timestamp)
            .bind(&event.action)
            .bind(&event.flow_path)
            .bind(&event.target)
            .bind(&event.actor)
            .bind(&event.source_ip)
            .bind(event.success)
            .bind(event.status_code as i32)
            .bind(metadata_str)
            .execute(&self.pool)
            .await
            .map_err(|e| IronCrewError::Validation(format!("PG insert audit: {}", e)))?;
        tracing::debug!("Audit event saved: {}", id);
        Ok(id)
    }

    async fn list_audit_events(
        &self,
        filter: &crate::engine::audit::AuditFilter,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<crate::engine::audit::AuditEvent>> {
        let WhereClause {
            sql: where_sql,
            params,
        } = store_sql::audit_where(filter, Dialect::Postgres);
        let mut sql = format!(
            "SELECT id, timestamp, action, flow_path, target, actor, source_ip, success, status_code, metadata::text
             FROM {}{}",
            self.audit_events_table, where_sql
        );
        sql.push_str(" ORDER BY timestamp DESC");
        if limit > 0 {
            sql.push_str(&format!(" LIMIT {}", limit));
        }
        if offset > 0 {
            sql.push_str(&format!(" OFFSET {}", offset));
        }

        let q = sqlx::query(sqlx::AssertSqlSafe(sql.to_string()));
        let q = bind_params(q, &params);

        let rows = q
            .fetch_all(&self.pool)
            .await
            .map_err(|e| IronCrewError::Validation(format!("PG list audit: {}", e)))?;

        let mut events = Vec::new();
        for row in rows {
            let metadata_str: Option<String> = row
                .try_get("metadata")
                .map_err(|e| IronCrewError::Validation(format!("Column error: {e}")))?;
            let metadata = metadata_str
                .as_deref()
                .map(|raw| decode_stored_json(raw, "audit_events.metadata"))
                .transpose()?;
            events.push(crate::engine::audit::AuditEvent {
                id: row
                    .try_get("id")
                    .map_err(|e| IronCrewError::Validation(e.to_string()))?,
                timestamp: row
                    .try_get("timestamp")
                    .map_err(|e| IronCrewError::Validation(e.to_string()))?,
                action: row
                    .try_get("action")
                    .map_err(|e| IronCrewError::Validation(e.to_string()))?,
                flow_path: row.try_get("flow_path").ok(),
                target: row.try_get("target").ok(),
                actor: row.try_get("actor").ok(),
                source_ip: row.try_get("source_ip").ok(),
                success: row
                    .try_get("success")
                    .map_err(|e| IronCrewError::Validation(e.to_string()))?,
                status_code: row
                    .try_get::<i32, _>("status_code")
                    .map_err(|e| IronCrewError::Validation(e.to_string()))?
                    as u16,
                metadata,
            });
        }
        Ok(events)
    }

    async fn count_audit_events(&self, filter: &crate::engine::audit::AuditFilter) -> Result<u64> {
        let WhereClause {
            sql: where_sql,
            params,
        } = store_sql::audit_where(filter, Dialect::Postgres);
        let sql = format!(
            "SELECT COUNT(*) FROM {}{}",
            self.audit_events_table, where_sql
        );

        let mut q = sqlx::query_scalar::<_, i64>(sqlx::AssertSqlSafe(sql.to_string()));
        for p in &params {
            q = match p {
                SqlParam::Text(s) => q.bind(s),
                SqlParam::Bool(b) => q.bind(b),
            };
        }

        let count = q
            .fetch_one(&self.pool)
            .await
            .map_err(|e| IronCrewError::Validation(format!("PG count audit: {}", e)))?;
        Ok(count as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_prefix_is_lowercase_and_identifier_safe() {
        assert!(validate_table_prefix("").is_ok());
        assert!(validate_table_prefix("project_42_").is_ok());
        assert!(validate_table_prefix(&"a".repeat(MAX_TABLE_PREFIX_BYTES)).is_ok());
        assert!(validate_table_prefix(&"a".repeat(MAX_TABLE_PREFIX_BYTES + 1)).is_err());
        assert!(validate_table_prefix("MixedCase_").is_err());
        assert!(validate_table_prefix("hyphen-").is_err());
    }
}
