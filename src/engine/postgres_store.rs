#![cfg(feature = "postgres")]

use std::time::Duration;

use async_trait::async_trait;
use serde::de::DeserializeOwned;
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Row};

use crate::utils::error::{IronCrewError, Result};

/// Upper bound on the per-retry backoff delay during store init.
const CONNECT_BACKOFF_CAP_MS: u64 = 30_000;
const MAX_DB_POOL_SIZE: u32 = 128;
const MAX_CONNECT_RETRIES: u32 = 100;
const MAX_CONNECT_TIMEOUT_SECS: u64 = 120;
const MAX_TABLE_PREFIX_BYTES: usize = 37;
const IDEMPOTENCY_COLUMNS: &str = "key_hash, principal_id, request_fingerprint, operation, scope, \
    resource_id, exclusive_scope, attempt_id, owner_instance_id, base_revision, state, \
    response_status, response_body, lease_expires_at, created_at, updated_at, completed_at, \
    expires_at, ttl_seconds";

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

fn decode_stored_json<T: DeserializeOwned>(raw: &str, field: &str) -> Result<T> {
    serde_json::from_str(raw).map_err(|error| {
        IronCrewError::Validation(format!(
            "PostgreSQL stored JSON in '{field}' has an invalid shape: {error}"
        ))
    })
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

fn parse_timestamp(label: &str, value: &str) -> Result<chrono::DateTime<chrono::Utc>> {
    chrono::DateTime::parse_from_rfc3339(value)
        .map(|timestamp| timestamp.with_timezone(&chrono::Utc))
        .map_err(|error| {
            IronCrewError::Validation(format!("{label} is not valid RFC3339: {error}"))
        })
}

fn canonical_timestamp(label: &str, value: &str) -> Result<String> {
    Ok(parse_timestamp(label, value)?.to_rfc3339_opts(chrono::SecondsFormat::Micros, true))
}

/// Exponential backoff delay before the next connection retry.
///
/// `attempt` is 1-based (1 = delay before the first retry). The delay doubles
/// each attempt, starting from `base_ms`, capped at `cap_ms`. Saturating math
/// keeps large attempt counts from overflowing.
fn retry_backoff(attempt: u32, base_ms: u64, cap_ms: u64) -> Duration {
    let factor = 1u64
        .checked_shl(attempt.saturating_sub(1))
        .unwrap_or(u64::MAX);
    Duration::from_millis(base_ms.saturating_mul(factor).min(cap_ms))
}

use super::idempotency::{
    CONVERSATION_MESSAGE_OPERATION, ConversationIdempotencyCommit, IdempotencyClaim,
    IdempotencyClaimOutcome, IdempotencyCompletion, IdempotencyCompletionOutcome,
    IdempotencyLimits, IdempotencyLookup, IdempotencyQuotaResource, IdempotencyQuotaScope,
    IdempotencyRecord, IdempotencyState, IdempotencyUsage, PrincipalId, RUN_OPERATION,
    RunFenceHeartbeat, validate_digest,
};
use super::run_history::{
    ListRunsFilter, RunCompletion, RunIntent, RunRecord, RunStatus, RunSummary, RunTransition,
};
use super::sessions::{ConversationRecord, ConversationSummary, DialogStateRecord};
use super::store::{RunLeaseConfig, StateStore};
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
    lease: RunLeaseConfig,
}

#[derive(Debug, Clone, Copy)]
struct IdempotencyAccounting {
    records: usize,
    in_flight: usize,
    response_bytes: usize,
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
        // Validate table prefix to prevent SQL injection via env var
        validate_table_prefix(table_prefix)?;

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

        let mut attempt: u32 = 0;
        let pool = loop {
            attempt += 1;
            match PgPoolOptions::new()
                .max_connections(max_conn)
                .acquire_timeout(Duration::from_secs(connect_timeout_secs))
                .connect(database_url)
                .await
            {
                Ok(pool) => break pool,
                Err(e) => {
                    if attempt > retries {
                        return Err(IronCrewError::Validation(format!(
                            "Failed to connect to PostgreSQL after {} attempt(s): {}",
                            attempt, e
                        )));
                    }
                    let delay = retry_backoff(attempt, backoff_base_ms, CONNECT_BACKOFF_CAP_MS);
                    tracing::warn!(
                        "PostgreSQL connection attempt {}/{} failed: {}; retrying in {:?}",
                        attempt,
                        retries + 1,
                        e,
                        delay
                    );
                    tokio::time::sleep(delay).await;
                }
            }
        };

        ensure_supported_postgres_version(&pool).await?;

        let table_name = format!("{}runs", table_prefix);
        let conversations_table = format!("{}conversations", table_prefix);
        let dialogs_table = format!("{}dialogs", table_prefix);
        let audit_events_table = format!("{}audit_events", table_prefix);
        let idempotency_table = format!("{}idempotency", table_prefix);
        let idempotency_accounting_table = format!("{}idempotency_accounting", table_prefix);

        let store = Self {
            pool,
            table_name: table_name.clone(),
            conversations_table,
            dialogs_table,
            audit_events_table,
            idempotency_table,
            idempotency_accounting_table,
            lease,
        };
        store.bootstrap().await?;

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

    async fn lock_idempotency_quota(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    ) -> Result<()> {
        self.lock_advisory(tx, "idempotency-quota", "global", false)
            .await
    }

    async fn lock_idempotency_key(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        key_hash: &str,
    ) -> Result<()> {
        self.lock_advisory(tx, "idempotency-key", key_hash, false)
            .await
    }

    async fn lock_idempotency_principal(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        principal_id: &PrincipalId,
    ) -> Result<()> {
        self.lock_advisory(tx, "idempotency-principal", principal_id.as_str(), false)
            .await
    }

    async fn lock_idempotency_scope(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        exclusive_scope: &str,
    ) -> Result<()> {
        self.lock_advisory(tx, "idempotency-scope", exclusive_scope, false)
            .await
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

    async fn idempotency_accounting_for_update(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        principal_id: &PrincipalId,
    ) -> Result<(IdempotencyAccounting, IdempotencyAccounting)> {
        let global_sql = format!(
            "SELECT record_count, in_flight_count, response_bytes FROM {} \
             WHERE principal_id = 'global' AND is_global = TRUE FOR UPDATE",
            self.idempotency_accounting_table
        );
        let global = sqlx::query_as(sqlx::AssertSqlSafe(global_sql))
            .fetch_optional(&mut **tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL global idempotency accounting lookup failed: {error}"
                ))
            })?
            .ok_or_else(|| {
                IronCrewError::Validation(
                    "PostgreSQL global idempotency accounting row is missing".into(),
                )
            })?;

        let principal_sql = format!(
            "SELECT record_count, in_flight_count, response_bytes FROM {} \
             WHERE principal_id = $1 AND is_global = FALSE FOR UPDATE",
            self.idempotency_accounting_table
        );
        let principal = sqlx::query_as(sqlx::AssertSqlSafe(principal_sql))
            .bind(principal_id.as_str())
            .fetch_optional(&mut **tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL principal idempotency accounting lookup failed: {error}"
                ))
            })?;
        Ok((
            decode_idempotency_accounting(global)?,
            principal
                .map(decode_idempotency_accounting)
                .transpose()?
                .unwrap_or(IdempotencyAccounting {
                    records: 0,
                    in_flight: 0,
                    response_bytes: 0,
                }),
        ))
    }

    async fn idempotency_retry_after_seconds(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        principal_id: Option<&PrincipalId>,
        resource: IdempotencyQuotaResource,
        database_now: &str,
    ) -> Result<u64> {
        let principal = principal_id.map(PrincipalId::as_str);
        let sql = match resource {
            IdempotencyQuotaResource::Records => format!(
                "SELECT GREATEST(1, COALESCE(CEIL(EXTRACT(EPOCH FROM (MIN(\
                     CASE WHEN state IN ('completed', 'indeterminate') \
                          THEN expires_at::timestamptz \
                          ELSE lease_expires_at::timestamptz + \
                               ttl_seconds * interval '1 second' END\
                 ) - $1::timestamptz)))::BIGINT, 1)) \
                 FROM {} WHERE ($2::TEXT IS NULL OR principal_id = $2)",
                self.idempotency_table
            ),
            IdempotencyQuotaResource::InFlight => format!(
                "SELECT GREATEST(1, COALESCE(CEIL(EXTRACT(EPOCH FROM (\
                     MIN(lease_expires_at::timestamptz) - $1::timestamptz\
                 )))::BIGINT, 1)) \
                 FROM {} WHERE state IN ('claimed', 'running') \
                   AND ($2::TEXT IS NULL OR principal_id = $2)",
                self.idempotency_table
            ),
        };
        let seconds: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
            .bind(database_now)
            .bind(principal)
            .fetch_one(&mut **tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL idempotency quota retry calculation failed: {error}"
                ))
            })?;
        u64::try_from(seconds).map_err(|_| {
            IronCrewError::Validation(
                "PostgreSQL idempotency quota retry delay is out of range".into(),
            )
        })
    }

    async fn get_idempotency_in_transaction(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        key_hash: &str,
    ) -> Result<Option<IdempotencyRecord>> {
        let sql = format!(
            "SELECT {IDEMPOTENCY_COLUMNS} FROM {} WHERE key_hash = $1 FOR UPDATE",
            self.idempotency_table
        );
        let row = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(key_hash)
            .fetch_optional(&mut **tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!("PostgreSQL idempotency lookup failed: {error}"))
            })?;
        row.as_ref().map(row_to_idempotency_record).transpose()
    }

    async fn idempotency_principal_for_key(&self, key_hash: &str) -> Result<Option<PrincipalId>> {
        let sql = format!(
            "SELECT principal_id FROM {} WHERE key_hash = $1",
            self.idempotency_table
        );
        let principal: Option<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
            .bind(key_hash)
            .fetch_optional(&self.pool)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL idempotency principal lookup failed: {error}"
                ))
            })?;
        principal.map(PrincipalId::from_digest).transpose()
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

    async fn mark_record_indeterminate_in_transaction(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        mut record: IdempotencyRecord,
    ) -> Result<IdempotencyRecord> {
        let (completed_at, expires_at) = self
            .database_clock_with_deadline(
                tx,
                record.ttl_seconds,
                "idempotency indeterminate transition",
            )
            .await?;
        let sql = format!(
            "UPDATE {} SET state = 'indeterminate', response_status = NULL, \
             response_body = NULL, lease_expires_at = '', updated_at = $1, \
             completed_at = $1, expires_at = $2 \
             WHERE key_hash = $3 AND attempt_id = $4 \
               AND state IN ('claimed', 'running')",
            self.idempotency_table
        );
        let result = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(&completed_at)
            .bind(&expires_at)
            .bind(&record.key_hash)
            .bind(&record.attempt_id)
            .execute(&mut **tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL idempotency indeterminate transition failed: {error}"
                ))
            })?;
        if result.rows_affected() != 1 {
            return Err(IronCrewError::Conflict(format!(
                "Idempotency claim '{}' changed before it could be fenced",
                record.key_hash
            )));
        }
        record.state = IdempotencyState::Indeterminate;
        record.response_status = None;
        record.response_body = None;
        record.lease_expires_at.clear();
        record.updated_at = completed_at.clone();
        record.completed_at = Some(completed_at);
        record.expires_at = Some(expires_at);
        record.validate()?;
        Ok(record)
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
                "CREATE UNIQUE INDEX IF NOT EXISTS {it}_scope_uidx \
                 ON {it} (exclusive_scope) \
                 WHERE exclusive_scope IS NOT NULL \
                   AND state IN ('claimed', 'running')"
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
            "PostgreSQL bootstrap complete for tables '{}', '{}', '{}', '{}', '{}', '{}'",
            self.table_name,
            self.conversations_table,
            self.dialogs_table,
            self.audit_events_table,
            self.idempotency_table,
            self.idempotency_accounting_table
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
        if idempotency_columns != 19 {
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
        let scope_index = format!("{}_scope_uidx", self.idempotency_table);
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
                 (idx.relname = $4 AND i.indisunique AND i.indnkeyatts = 1 \
                   AND pg_get_indexdef(i.indexrelid, 1, TRUE) = 'exclusive_scope' \
                   AND i.indpred IS NOT NULL \
                   AND pg_get_expr(i.indpred, i.indrelid) LIKE '%exclusive_scope IS NOT NULL%' \
                   AND pg_get_expr(i.indpred, i.indrelid) LIKE '%claimed%' \
                   AND pg_get_expr(i.indpred, i.indrelid) LIKE '%running%')\
               )",
        )
        .bind(&self.idempotency_table)
        .bind(&expires_index)
        .bind(&resource_index)
        .bind(&scope_index)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| {
            IronCrewError::Validation(format!(
                "Failed to verify PostgreSQL idempotency indexes: {e}"
            ))
        })?;
        if valid_idempotency_indexes != 3 {
            return Err(IronCrewError::Validation(format!(
                "PostgreSQL table '{}' is missing one or more required idempotency indexes",
                self.idempotency_table
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
        Ok(())
    }
}

async fn ensure_supported_postgres_version(pool: &PgPool) -> Result<()> {
    let version_str: String = sqlx::query("SHOW server_version_num")
        .fetch_one(pool)
        .await
        .map_err(|e| {
            IronCrewError::Validation(format!(
                "Failed to determine PostgreSQL server version: {}",
                e
            ))
        })?
        .try_get(0)
        .map_err(|e| IronCrewError::Validation(format!("Invalid PostgreSQL version row: {}", e)))?;

    let version_num: i32 = version_str.parse().map_err(|e| {
        IronCrewError::Validation(format!(
            "Failed to parse PostgreSQL server_version_num '{}': {}",
            version_str, e
        ))
    })?;

    if version_num < 150000 {
        return Err(IronCrewError::Validation(format!(
            "PostgreSQL 15+ is required; connected server reports version {}. \
IronCrew relies on PostgreSQL 15 features for flow-scoped session uniqueness \
and targets extension-capable deployments such as pgvector-enabled installs.",
            version_str
        )));
    }

    Ok(())
}

#[async_trait]
impl StateStore for PostgresStore {
    async fn save_run_intent(&self, intent: RunIntent) -> Result<String> {
        let run_id = intent
            .suggested_id
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let tags_json = serde_json::to_string(&intent.tags)
            .map_err(|e| IronCrewError::Validation(format!("Tags serialize: {}", e)))?;
        let empty_tasks = serde_json::to_string(&serde_json::Value::Array(Vec::new()))
            .map_err(|e| IronCrewError::Validation(format!("Empty tasks serialize: {}", e)))?;
        let sql = format!(
            "INSERT INTO {} (run_id, flow_name, flow, status, started_at, finished_at, duration_ms, task_results, agent_count, task_count, total_tokens, cached_tokens, tags, owner_instance_id, lease_expires_at)
             VALUES ($1, $2, $3, 'running', $4, '', 0, $5::jsonb, $6, $7, 0, 0, $8::jsonb, $9, $10)
             ON CONFLICT (run_id) DO NOTHING",
            self.table_name
        );
        let mut tx = self.pool.begin().await.map_err(|error| {
            IronCrewError::Validation(format!("PG insert intent transaction: {error}"))
        })?;
        self.lock_run_fence(&mut tx, true).await?;
        self.lock_resource(&mut tx, RUN_OPERATION, "", &run_id)
            .await?;
        let (database_now, lease_expires_at) = self
            .database_clock_with_deadline(&mut tx, self.lease.ttl().as_secs(), "run intent lease")
            .await?;
        let inserted = sqlx::query(sqlx::AssertSqlSafe(sql.to_string()))
            .bind(&run_id)
            .bind(&intent.flow_name)
            .bind(&intent.flow)
            .bind(&intent.started_at)
            .bind(&empty_tasks)
            .bind(intent.agent_count as i64)
            .bind(intent.task_count as i64)
            .bind(&tags_json)
            .bind(self.lease.instance_id())
            .bind(&lease_expires_at)
            .execute(&mut *tx)
            .await
            .map_err(|e| IronCrewError::Validation(format!("PG insert intent: {}", e)))?;
        if inserted.rows_affected() == 0 {
            let hydrate_sql = format!(
                "UPDATE {runs} AS run SET \
                     flow_name = $1, agent_count = $2, task_count = $3, \
                     tags = $4::jsonb, lease_expires_at = $5 \
                 WHERE run.run_id = $6 AND run.flow = $7 \
                   AND run.owner_instance_id = $8 \
                   AND run.status IN ('running', 'waiting_for_input') \
                   AND EXISTS (\
                       SELECT 1 FROM {idempotency} AS idem \
                       WHERE idem.operation = $9 AND idem.scope = $7 \
                         AND idem.resource_id = $6 \
                         AND idem.owner_instance_id = $8 \
                         AND idem.state IN ('running', 'completed')\
                   )",
                runs = self.table_name,
                idempotency = self.idempotency_table
            );
            let hydrated = sqlx::query(sqlx::AssertSqlSafe(hydrate_sql))
                .bind(&intent.flow_name)
                .bind(intent.agent_count as i64)
                .bind(intent.task_count as i64)
                .bind(&tags_json)
                .bind(&lease_expires_at)
                .bind(&run_id)
                .bind(&intent.flow)
                .bind(self.lease.instance_id())
                .bind(RUN_OPERATION)
                .execute(&mut *tx)
                .await
                .map_err(|error| {
                    IronCrewError::Validation(format!(
                        "PG idempotent provisional run hydration: {error}"
                    ))
                })?;
            if hydrated.rows_affected() != 1 {
                return Err(IronCrewError::Conflict(format!(
                    "Run '{run_id}' already exists without a matching idempotent provisional intent"
                )));
            }
        }
        let mapping_sql = format!(
            "UPDATE {} SET state = 'running', lease_expires_at = $1, updated_at = $2 \
             WHERE operation = $3 AND scope = $4 AND resource_id = $5 \
               AND owner_instance_id = $6 AND state = 'claimed'",
            self.idempotency_table
        );
        sqlx::query(sqlx::AssertSqlSafe(mapping_sql))
            .bind(&lease_expires_at)
            .bind(&database_now)
            .bind(RUN_OPERATION)
            .bind(&intent.flow)
            .bind(&run_id)
            .bind(self.lease.instance_id())
            .execute(&mut *tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!("PG run idempotency mapping transition: {error}"))
            })?;
        tx.commit().await.map_err(|error| {
            IronCrewError::Validation(format!("PG insert intent commit: {error}"))
        })?;
        tracing::debug!("Run intent saved: {}", run_id);
        Ok(run_id)
    }

    async fn update_run_completion(
        &self,
        run_id: &str,
        completion: RunCompletion,
    ) -> Result<RunTransition> {
        completion.validate()?;
        let task_results_json = serde_json::to_string(&completion.task_results)
            .map_err(|e| IronCrewError::Validation(format!("task_results serialize: {}", e)))?;
        let mut tx = self.pool.begin().await.map_err(|error| {
            IronCrewError::Validation(format!("PG update completion transaction: {error}"))
        })?;
        self.lock_idempotency_quota(&mut tx).await?;
        self.lock_run_fence(&mut tx, true).await?;
        self.lock_resource(&mut tx, RUN_OPERATION, "", run_id)
            .await?;
        let (database_now, _) = self
            .database_clock_with_deadline(&mut tx, 0, "run completion")
            .await?;
        let sql = format!(
            "UPDATE {}
             SET status = $1, finished_at = $2, duration_ms = $3,
                 task_results = $4::jsonb, total_tokens = $5, cached_tokens = $6,
                 lease_expires_at = ''
             WHERE run_id = $7 AND status IN ('running', 'waiting_for_input')
               AND owner_instance_id = $8",
            self.table_name
        );
        let result = sqlx::query(sqlx::AssertSqlSafe(sql.to_string()))
            .bind(completion.status.to_string())
            .bind(&completion.finished_at)
            .bind(completion.duration_ms as i64)
            .bind(&task_results_json)
            .bind(completion.total_tokens as i32)
            .bind(completion.cached_tokens as i32)
            .bind(run_id)
            .bind(self.lease.instance_id())
            .execute(&mut *tx)
            .await
            .map_err(|e| IronCrewError::Validation(format!("PG update completion: {}", e)))?;

        let transition = if result.rows_affected() == 0 {
            let sql = format!(
                "SELECT status, owner_instance_id, finished_at FROM {} WHERE run_id = $1 FOR UPDATE",
                self.table_name
            );
            let row = sqlx::query(sqlx::AssertSqlSafe(sql.to_string()))
                .bind(run_id)
                .fetch_optional(&mut *tx)
                .await
                .map_err(|e| {
                    IronCrewError::Validation(format!("PG completion state query: {}", e))
                })?;
            let Some(row) = row else {
                return Err(IronCrewError::Validation(format!(
                    "Run '{}' not found",
                    run_id
                )));
            };
            let status: String = row
                .try_get("status")
                .map_err(|e| IronCrewError::Validation(format!("Column error: {}", e)))?;
            let parsed = status.parse::<RunStatus>()?;
            if parsed.is_terminal() {
                RunTransition::AlreadyTerminal(parsed)
            } else {
                let owner: String = row
                    .try_get("owner_instance_id")
                    .map_err(|e| IronCrewError::Validation(format!("Column error: {}", e)))?;
                return Err(IronCrewError::Validation(format!(
                    "Run '{}' is owned by instance '{}', not '{}'",
                    run_id,
                    owner,
                    self.lease.instance_id()
                )));
            }
        } else {
            RunTransition::Applied
        };

        let mapping_sql = format!(
            "UPDATE {} SET state = 'completed', lease_expires_at = '', \
             updated_at = $1, completed_at = $1, \
             expires_at = to_char(\
                 ($1::timestamptz + ttl_seconds * interval '1 second') AT TIME ZONE 'UTC', \
                 'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"'\
             ) \
             WHERE operation = $2 AND resource_id = $3 \
               AND state IN ('claimed', 'running', 'indeterminate')",
            self.idempotency_table
        );
        sqlx::query(sqlx::AssertSqlSafe(mapping_sql))
            .bind(&database_now)
            .bind(RUN_OPERATION)
            .bind(run_id)
            .execute(&mut *tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PG run idempotency completion transition: {error}"
                ))
            })?;
        tx.commit().await.map_err(|error| {
            IronCrewError::Validation(format!("PG update completion commit: {error}"))
        })?;
        tracing::info!("Run completion saved: {} ({})", run_id, completion.status);
        Ok(transition)
    }

    async fn update_run_status(
        &self,
        run_id: &str,
        status: crate::engine::run_history::RunStatus,
    ) -> Result<()> {
        if !status.is_in_flight() {
            return Err(IronCrewError::Validation(format!(
                "update_run_status requires an in-flight status, got '{}'",
                status
            )));
        }
        let sql = format!(
            "UPDATE {} SET status = $1
             WHERE run_id = $2 AND status IN ('running', 'waiting_for_input')
               AND owner_instance_id = $3",
            self.table_name
        );
        let result = sqlx::query(sqlx::AssertSqlSafe(sql.to_string()))
            .bind(status.to_string())
            .bind(run_id)
            .bind(self.lease.instance_id())
            .execute(&self.pool)
            .await
            .map_err(|e| IronCrewError::Validation(format!("PG update status: {}", e)))?;
        if result.rows_affected() == 0 {
            return Err(IronCrewError::Validation(format!(
                "Run '{}' not found or not in an in-flight state",
                run_id
            )));
        }
        Ok(())
    }

    fn instance_id(&self) -> &str {
        self.lease.instance_id()
    }

    fn run_lease_ttl(&self) -> Duration {
        self.lease.ttl()
    }

    async fn heartbeat_owned_runs(&self) -> Result<usize> {
        let mut tx = self.pool.begin().await.map_err(|error| {
            IronCrewError::Validation(format!("PG heartbeat transaction: {error}"))
        })?;
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
        self.lock_idempotency_quota(&mut tx).await?;
        self.lock_run_fence(&mut tx, false).await?;
        let (database_now, _) = self
            .database_clock_with_deadline(&mut tx, 0, "run reconciliation")
            .await?;

        // A process may die after durably allocating/replying with a run id
        // but before publishing the normal run intent. Materialize a minimal
        // terminal run so retries can replay that id without re-executing and
        // callers can observe an explicit Abandoned outcome instead of 404.
        let fallback_sql = format!(
            "INSERT INTO {runs} (\
                 run_id, flow_name, flow, status, started_at, finished_at, duration_ms, \
                 task_results, agent_count, task_count, total_tokens, cached_tokens, tags, \
                 owner_instance_id, lease_expires_at\
             ) \
             SELECT idem.resource_id, idem.scope, idem.scope, 'abandoned', \
                    idem.created_at, $1, 0, '[]'::jsonb, 0, 0, 0, 0, '[]'::jsonb, \
                    idem.owner_instance_id, '' \
             FROM {idempotency} AS idem \
             WHERE idem.operation = $2 AND idem.state = 'claimed' \
               AND idem.lease_expires_at::timestamptz <= $3::timestamptz \
               AND NOT EXISTS (SELECT 1 FROM {runs} AS run WHERE run.run_id = idem.resource_id) \
             ON CONFLICT (run_id) DO NOTHING",
            runs = self.table_name,
            idempotency = self.idempotency_table
        );
        let inserted = sqlx::query(sqlx::AssertSqlSafe(fallback_sql))
            .bind(&database_now)
            .bind(RUN_OPERATION)
            .bind(&database_now)
            .execute(&mut *tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!("PG idempotent run fallback: {error}"))
            })?;

        let sql = format!(
            "UPDATE {}
             SET status = 'abandoned', finished_at = $1, lease_expires_at = ''
             WHERE status IN ('running', 'waiting_for_input')
               AND (lease_expires_at = '' \
                    OR lease_expires_at::timestamptz <= $2::timestamptz)",
            self.table_name
        );
        let result = sqlx::query(sqlx::AssertSqlSafe(sql.to_string()))
            .bind(&database_now)
            .bind(&database_now)
            .execute(&mut *tx)
            .await
            .map_err(|e| IronCrewError::Validation(format!("PG reconcile: {}", e)))?;

        let mapping_sql = format!(
            "UPDATE {idempotency} AS idem \
             SET state = 'completed', lease_expires_at = '', \
                 updated_at = $2, completed_at = $2, \
                 expires_at = to_char(\
                     ($2::timestamptz + idem.ttl_seconds * interval '1 second') \
                         AT TIME ZONE 'UTC', \
                     'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"'\
                 ) \
             FROM {runs} AS run \
             WHERE idem.operation = $1 AND idem.resource_id = run.run_id \
               AND idem.state IN ('claimed', 'running', 'indeterminate') \
               AND run.status NOT IN ('running', 'waiting_for_input')",
            idempotency = self.idempotency_table,
            runs = self.table_name
        );
        sqlx::query(sqlx::AssertSqlSafe(mapping_sql))
            .bind(RUN_OPERATION)
            .bind(&database_now)
            .execute(&mut *tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PG reconciled run idempotency transition: {error}"
                ))
            })?;

        // Conversation effects cannot be reconstructed after their exclusive
        // lease expires. Preserve a terminal tombstone so the same request is
        // never executed a second time.
        let conversation_sql = format!(
            "UPDATE {} SET state = 'indeterminate', response_status = NULL, \
             response_body = NULL, lease_expires_at = '', updated_at = $1, \
             completed_at = $1, expires_at = to_char(\
                 ($1::timestamptz + ttl_seconds * interval '1 second') AT TIME ZONE 'UTC', \
                 'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"'\
             ) \
             WHERE operation = $2 AND state IN ('claimed', 'running') \
               AND lease_expires_at::timestamptz <= $1::timestamptz",
            self.idempotency_table
        );
        sqlx::query(sqlx::AssertSqlSafe(conversation_sql))
            .bind(&database_now)
            .bind(CONVERSATION_MESSAGE_OPERATION)
            .execute(&mut *tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PG conversation idempotency reconciliation: {error}"
                ))
            })?;
        tx.commit()
            .await
            .map_err(|error| IronCrewError::Validation(format!("PG reconcile commit: {error}")))?;
        Ok((inserted.rows_affected() + result.rows_affected()) as usize)
    }

    async fn get_run(&self, run_id: &str) -> Result<RunRecord> {
        let sql = format!(
            "SELECT run_id, flow_name, flow, status, started_at, finished_at, duration_ms, task_results::text, agent_count, task_count, total_tokens, cached_tokens, tags::text, owner_instance_id, lease_expires_at
             FROM {} WHERE run_id = $1",
            self.table_name
        );

        let row = sqlx::query(sqlx::AssertSqlSafe(sql.to_string()))
            .bind(run_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| IronCrewError::Validation(format!("PostgreSQL query error: {}", e)))?
            .ok_or_else(|| IronCrewError::Validation(format!("Run '{}' not found", run_id)))?;

        row_to_record(&row)
    }

    async fn list_runs_summary(
        &self,
        filter: &ListRunsFilter,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<RunSummary>> {
        // Shared WHERE builder keeps the tag containment identical to the
        // SQLite backend. We NEVER select task_results — that's the whole
        // point of the summary view. LIMIT/OFFSET stay inline (trusted
        // integers) so the builder's `$N` numbering is left undisturbed.
        let WhereClause {
            sql: where_sql,
            params,
        } = store_sql::runs_where(filter, Dialect::Postgres);
        let mut sql = format!(
            "SELECT run_id, flow_name, flow, status, started_at, finished_at, duration_ms, \
             agent_count, task_count, total_tokens, cached_tokens, tags::text \
             FROM {}{}",
            self.table_name, where_sql
        );
        sql.push_str(" ORDER BY started_at DESC");
        if limit > 0 {
            sql.push_str(&format!(" LIMIT {}", limit as i64));
            if offset > 0 {
                sql.push_str(&format!(" OFFSET {}", offset as i64));
            }
        }

        let query = sqlx::query(sqlx::AssertSqlSafe(sql.to_string()));
        let query = bind_params(query, &params);

        let rows = query
            .fetch_all(&self.pool)
            .await
            .map_err(|e| IronCrewError::Validation(format!("PostgreSQL query error: {}", e)))?;

        rows.iter().map(row_to_summary).collect()
    }

    async fn count_runs(&self, filter: &ListRunsFilter) -> Result<u64> {
        let WhereClause {
            sql: where_sql,
            params,
        } = store_sql::runs_where(filter, Dialect::Postgres);
        let sql = format!("SELECT COUNT(*) FROM {}{}", self.table_name, where_sql);

        let query = sqlx::query(sqlx::AssertSqlSafe(sql.to_string()));
        let query = bind_params(query, &params);

        let row = query
            .fetch_one(&self.pool)
            .await
            .map_err(|e| IronCrewError::Validation(format!("PostgreSQL count error: {}", e)))?;
        let count: i64 = row
            .try_get(0)
            .map_err(|e| IronCrewError::Validation(format!("Column error: {}", e)))?;
        Ok(count as u64)
    }

    async fn delete_run(&self, run_id: &str) -> Result<()> {
        let sql = format!("DELETE FROM {} WHERE run_id = $1", self.table_name);

        let result = sqlx::query(sqlx::AssertSqlSafe(sql.to_string()))
            .bind(run_id)
            .execute(&self.pool)
            .await
            .map_err(|e| IronCrewError::Validation(format!("PostgreSQL delete error: {}", e)))?;

        if result.rows_affected() == 0 {
            return Err(IronCrewError::Validation(format!(
                "Run '{}' not found",
                run_id
            )));
        }
        Ok(())
    }

    async fn lookup_idempotency_for_principal(
        &self,
        principal_id: &PrincipalId,
        key_hash: &str,
        request_fingerprint: &str,
        now: &str,
    ) -> Result<IdempotencyLookup> {
        principal_id.validate()?;
        validate_digest("idempotency key hash", key_hash)?;
        validate_digest("request fingerprint", request_fingerprint)?;
        parse_timestamp("idempotency lookup time", now)?;
        let mut tx = self.pool.begin().await.map_err(|error| {
            IronCrewError::Validation(format!(
                "PostgreSQL idempotency lookup transaction failed: {error}"
            ))
        })?;
        self.lock_idempotency_key(&mut tx, key_hash).await?;
        let (database_now, _) = self
            .database_clock_with_deadline(&mut tx, 0, "idempotency lookup")
            .await?;
        let now_timestamp = parse_timestamp("PostgreSQL idempotency clock", &database_now)?;

        let Some(record) = self
            .get_idempotency_in_transaction(&mut tx, key_hash)
            .await?
        else {
            tx.commit().await.map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL idempotency lookup commit failed: {error}"
                ))
            })?;
            return Ok(IdempotencyLookup::Miss);
        };

        let outcome = if &record.principal_id != principal_id
            || record.request_fingerprint != request_fingerprint
        {
            IdempotencyLookup::Conflict
        } else if record.state.is_terminal()
            && record
                .expires_at
                .as_deref()
                .map(|expires_at| {
                    parse_timestamp("stored idempotency retention expiry", expires_at)
                        .map(|expires_at| expires_at <= now_timestamp)
                })
                .transpose()?
                .unwrap_or(false)
        {
            IdempotencyLookup::Miss
        } else if record.state == IdempotencyState::Indeterminate {
            IdempotencyLookup::Indeterminate(record)
        } else if record.replayable() {
            IdempotencyLookup::Replay(record)
        } else {
            match record.state {
                IdempotencyState::Claimed | IdempotencyState::Running => {
                    let lease = parse_timestamp(
                        "stored idempotency lease expiry",
                        &record.lease_expires_at,
                    )?;
                    if lease > now_timestamp {
                        IdempotencyLookup::InProgress(record)
                    } else {
                        IdempotencyLookup::Indeterminate(record)
                    }
                }
                IdempotencyState::Completed | IdempotencyState::Indeterminate => {
                    IdempotencyLookup::Indeterminate(record)
                }
            }
        };
        tx.commit().await.map_err(|error| {
            IronCrewError::Validation(format!(
                "PostgreSQL idempotency lookup commit failed: {error}"
            ))
        })?;
        Ok(outcome)
    }

    async fn claim_idempotency_with_limits(
        &self,
        claim: IdempotencyClaim,
        limits: IdempotencyLimits,
    ) -> Result<IdempotencyClaimOutcome> {
        claim.validate()?;
        limits.validate()?;
        let mut tx = self.pool.begin().await.map_err(|error| {
            IronCrewError::Validation(format!(
                "PostgreSQL idempotency claim transaction failed: {error}"
            ))
        })?;
        // Every capacity mutation follows one order: global quota, principal,
        // optional run fence, resource, exclusive scope, and finally key.
        self.lock_idempotency_quota(&mut tx).await?;
        self.lock_idempotency_principal(&mut tx, &claim.principal_id)
            .await?;
        if claim.operation == RUN_OPERATION {
            self.lock_run_fence(&mut tx, true).await?;
        }
        self.lock_resource(
            &mut tx,
            &claim.operation,
            if claim.operation == RUN_OPERATION {
                ""
            } else {
                &claim.scope
            },
            &claim.resource_id,
        )
        .await?;
        let mut recovery_hazard_key = None;
        if let Some(exclusive_scope) = claim.exclusive_scope.as_deref() {
            self.lock_idempotency_scope(&mut tx, exclusive_scope)
                .await?;
        }
        self.lock_idempotency_key(&mut tx, &claim.key_hash).await?;
        let (database_now, lease_expires_at) = self
            .database_clock_with_deadline(&mut tx, self.lease.ttl().as_secs(), "idempotency claim")
            .await?;
        let database_timestamp = parse_timestamp("PostgreSQL idempotency clock", &database_now)?;

        if limits.prune_batch > 0 {
            let sql = format!(
                "DELETE FROM {table} WHERE key_hash IN (\
                     SELECT key_hash FROM {table} \
                     WHERE state IN ('completed', 'indeterminate') \
                       AND expires_at IS NOT NULL \
                       AND expires_at::timestamptz <= $1::timestamptz \
                     ORDER BY expires_at::timestamptz, key_hash LIMIT $2\
                 )",
                table = self.idempotency_table
            );
            sqlx::query(sqlx::AssertSqlSafe(sql))
                .bind(&database_now)
                .bind(i64::try_from(limits.prune_batch).unwrap_or(i64::MAX))
                .execute(&mut *tx)
                .await
                .map_err(|error| {
                    IronCrewError::Validation(format!(
                        "PostgreSQL idempotency claim pruning failed: {error}"
                    ))
                })?;
        }

        if let Some(record) = self
            .get_idempotency_in_transaction(&mut tx, &claim.key_hash)
            .await?
        {
            let expired_terminal = record.state.is_terminal()
                && record
                    .expires_at
                    .as_deref()
                    .map(|expires_at| {
                        parse_timestamp("stored idempotency retention expiry", expires_at)
                            .map(|expires_at| expires_at <= database_timestamp)
                    })
                    .transpose()?
                    .unwrap_or(false);
            if expired_terminal {
                let sql = format!("DELETE FROM {} WHERE key_hash = $1", self.idempotency_table);
                sqlx::query(sqlx::AssertSqlSafe(sql))
                    .bind(&claim.key_hash)
                    .execute(&mut *tx)
                    .await
                    .map_err(|error| {
                        IronCrewError::Validation(format!(
                            "PostgreSQL expired idempotency delete failed: {error}"
                        ))
                    })?;
            } else {
                let outcome = if record.principal_id != claim.principal_id
                    || record.request_fingerprint != claim.request_fingerprint
                {
                    IdempotencyClaimOutcome::Conflict
                } else if record.state == IdempotencyState::Indeterminate {
                    IdempotencyClaimOutcome::Indeterminate(record)
                } else if record.replayable() {
                    IdempotencyClaimOutcome::Replay(record)
                } else if record.state.is_in_flight()
                    && parse_timestamp("stored idempotency lease expiry", &record.lease_expires_at)?
                        <= database_timestamp
                {
                    let record = self
                        .mark_record_indeterminate_in_transaction(&mut tx, record)
                        .await?;
                    IdempotencyClaimOutcome::Indeterminate(record)
                } else if record.state.is_in_flight() {
                    IdempotencyClaimOutcome::InProgress(record)
                } else {
                    IdempotencyClaimOutcome::Indeterminate(record)
                };
                tx.commit().await.map_err(|error| {
                    IronCrewError::Validation(format!(
                        "PostgreSQL idempotency claim commit failed: {error}"
                    ))
                })?;
                return Ok(outcome);
            }
        }

        if claim.operation == CONVERSATION_MESSAGE_OPERATION {
            let expected_revision = claim.base_revision.ok_or_else(|| {
                IronCrewError::Validation(
                    "Conversation idempotency claim has no base revision".into(),
                )
            })?;
            let revision_sql = format!(
                "SELECT revision FROM {} \
                 WHERE id = $1 AND flow_path IS NOT DISTINCT FROM $2 FOR UPDATE",
                self.conversations_table
            );
            let current_revision: Option<i64> =
                sqlx::query_scalar(sqlx::AssertSqlSafe(revision_sql))
                    .bind(&claim.resource_id)
                    .bind(&claim.scope)
                    .fetch_optional(&mut *tx)
                    .await
                    .map_err(|error| {
                        IronCrewError::Validation(format!(
                            "PostgreSQL conversation idempotency revision query failed: {error}"
                        ))
                    })?;
            let current_revision = current_revision
                .map(u64::try_from)
                .transpose()
                .map_err(|_| {
                    IronCrewError::Validation("PostgreSQL conversation revision is negative".into())
                })?
                .unwrap_or(0);
            if current_revision != expected_revision {
                tx.commit().await.map_err(|error| {
                    IronCrewError::Validation(format!(
                        "PostgreSQL conversation idempotency conflict commit failed: {error}"
                    ))
                })?;
                return Ok(IdempotencyClaimOutcome::Conflict);
            }
        }

        if let Some(exclusive_scope) = claim.exclusive_scope.as_deref() {
            let sql = format!(
                "SELECT {IDEMPOTENCY_COLUMNS} FROM {} \
                 WHERE exclusive_scope = $1 AND key_hash <> $2 \
                   AND state IN ('claimed', 'running') FOR UPDATE",
                self.idempotency_table
            );
            if let Some(row) = sqlx::query(sqlx::AssertSqlSafe(sql))
                .bind(exclusive_scope)
                .bind(&claim.key_hash)
                .fetch_optional(&mut *tx)
                .await
                .map_err(|error| {
                    IronCrewError::Validation(format!(
                        "PostgreSQL idempotency exclusive-scope lookup failed: {error}"
                    ))
                })?
            {
                let record = row_to_idempotency_record(&row)?;
                let lease =
                    parse_timestamp("stored idempotency lease expiry", &record.lease_expires_at)?;
                if lease > database_timestamp {
                    tx.commit().await.map_err(|error| {
                        IronCrewError::Validation(format!(
                            "PostgreSQL idempotency busy commit failed: {error}"
                        ))
                    })?;
                    return Ok(IdempotencyClaimOutcome::Busy);
                }
                self.mark_record_indeterminate_in_transaction(&mut tx, record)
                    .await?;
                tx.commit().await.map_err(|error| {
                    IronCrewError::Validation(format!(
                        "PostgreSQL expired idempotency barrier commit failed: {error}"
                    ))
                })?;
                return Ok(IdempotencyClaimOutcome::Busy);
            }

            let hazard_sql = format!(
                "SELECT key_hash, principal_id, completed_at FROM {} \
                 WHERE exclusive_scope = $1 AND key_hash <> $2 \
                   AND state = 'indeterminate' \
                 ORDER BY completed_at, key_hash LIMIT 2 FOR UPDATE",
                self.idempotency_table
            );
            let hazard_rows = sqlx::query(sqlx::AssertSqlSafe(hazard_sql))
                .bind(exclusive_scope)
                .bind(&claim.key_hash)
                .fetch_all(&mut *tx)
                .await
                .map_err(|error| {
                    IronCrewError::Validation(format!(
                        "PostgreSQL indeterminate exclusive-scope lookup failed: {error}"
                    ))
                })?;
            if !hazard_rows.is_empty() {
                let hazard = if hazard_rows.len() == 1 {
                    let key_hash =
                        hazard_rows[0]
                            .try_get::<String, _>("key_hash")
                            .map_err(|error| {
                                IronCrewError::Validation(format!(
                                    "PostgreSQL idempotency hazard key decode failed: {error}"
                                ))
                            })?;
                    let principal_id = hazard_rows[0]
                        .try_get::<String, _>("principal_id")
                        .map(PrincipalId::from_digest)
                        .map_err(|error| {
                            IronCrewError::Validation(format!(
                                "PostgreSQL idempotency hazard principal decode failed: {error}"
                            ))
                        })??;
                    let completed_at = hazard_rows[0]
                        .try_get::<Option<String>, _>("completed_at")
                        .map_err(|error| {
                            IronCrewError::Validation(format!(
                                "PostgreSQL idempotency hazard timestamp decode failed: {error}"
                            ))
                        })?;
                    Some((key_hash, principal_id, completed_at))
                } else {
                    None
                };
                let grace = chrono::Duration::from_std(self.lease.ttl()).map_err(|_| {
                    IronCrewError::Validation(
                        "PostgreSQL idempotency recovery grace is out of range".into(),
                    )
                })?;
                let grace_elapsed = hazard
                    .as_ref()
                    .and_then(|(_, _, completed_at)| completed_at.as_deref())
                    .map(|completed_at| {
                        parse_timestamp("stored idempotency hazard completion", completed_at)
                    })
                    .transpose()?
                    .and_then(|completed_at| completed_at.checked_add_signed(grace))
                    .is_some_and(|recovery_at| recovery_at <= database_timestamp);
                let recoverable = hazard.as_ref().is_some_and(|(key_hash, principal_id, _)| {
                    principal_id == &claim.principal_id
                        && claim.recovery_key_hash.as_deref() == Some(key_hash.as_str())
                }) && grace_elapsed;
                if !recoverable {
                    tx.commit().await.map_err(|error| {
                        IronCrewError::Validation(format!(
                            "PostgreSQL idempotency hazard commit failed: {error}"
                        ))
                    })?;
                    return Ok(IdempotencyClaimOutcome::Busy);
                }
                let recovery_key_hash = claim.recovery_key_hash.as_deref().ok_or_else(|| {
                    IronCrewError::Validation("Missing idempotency recovery key".into())
                })?;
                // Keep the locked hazard bound until every quota check has
                // passed. A quota-denied transaction is committed so bounded
                // pruning/accounting can progress; clearing here would make
                // that commit consume the recovery capability without
                // inserting its successor.
                recovery_hazard_key = Some(recovery_key_hash.to_string());
            }
        }

        let (global_usage, principal_usage) = self
            .idempotency_accounting_for_update(&mut tx, &claim.principal_id)
            .await?;
        let quota = if global_usage.records >= limits.global_max_records {
            Some((
                IdempotencyQuotaScope::Global,
                IdempotencyQuotaResource::Records,
                None,
            ))
        } else if principal_usage.records >= limits.principal_max_records {
            Some((
                IdempotencyQuotaScope::Principal,
                IdempotencyQuotaResource::Records,
                Some(&claim.principal_id),
            ))
        } else if principal_usage.in_flight >= limits.principal_max_in_flight {
            Some((
                IdempotencyQuotaScope::Principal,
                IdempotencyQuotaResource::InFlight,
                Some(&claim.principal_id),
            ))
        } else {
            None
        };
        if let Some((scope, resource, retry_principal)) = quota {
            let retry_after_seconds = self
                .idempotency_retry_after_seconds(&mut tx, retry_principal, resource, &database_now)
                .await?;
            tx.commit().await.map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL idempotency quota commit failed: {error}"
                ))
            })?;
            return Ok(IdempotencyClaimOutcome::QuotaExceeded {
                scope,
                resource,
                retry_after_seconds,
            });
        }

        let mut record = claim.to_record();
        record.lease_expires_at = lease_expires_at;
        record.created_at = database_now.clone();
        record.updated_at = database_now;
        if let Some(response_body) = record.response_body.as_ref() {
            let response_fits = global_usage
                .response_bytes
                .checked_add(response_body.len())
                .is_some_and(|total| total <= limits.global_max_response_bytes)
                && principal_usage
                    .response_bytes
                    .checked_add(response_body.len())
                    .is_some_and(|total| total <= limits.principal_max_response_bytes);
            if !response_fits {
                record.response_body = None;
            }
        }
        record.validate()?;
        if let Some(hazard_key) = recovery_hazard_key.as_deref() {
            let clear_sql = format!(
                "UPDATE {} SET exclusive_scope = NULL, updated_at = $1 \
                 WHERE key_hash = $2 AND exclusive_scope = $3 \
                   AND principal_id = $4 AND state = 'indeterminate'",
                self.idempotency_table
            );
            let cleared = sqlx::query(sqlx::AssertSqlSafe(clear_sql))
                .bind(&record.created_at)
                .bind(hazard_key)
                .bind(record.exclusive_scope.as_deref())
                .bind(record.principal_id.as_str())
                .execute(&mut *tx)
                .await
                .map_err(|error| {
                    IronCrewError::Validation(format!(
                        "PostgreSQL idempotency hazard recovery failed: {error}"
                    ))
                })?;
            if cleared.rows_affected() != 1 {
                return Err(IronCrewError::Conflict(
                    "Idempotency recovery hazard changed before claim insertion".into(),
                ));
            }
        }
        let base_revision = record
            .base_revision
            .map(i64::try_from)
            .transpose()
            .map_err(|_| {
                IronCrewError::Validation(
                    "Idempotency base revision is out of PostgreSQL range".into(),
                )
            })?;
        let sql = format!(
            "INSERT INTO {} ({IDEMPOTENCY_COLUMNS}) VALUES (\
                 $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, 'claimed', \
                 $11, $12, $13, $14, $14, NULL, NULL, $15\
             )",
            self.idempotency_table
        );
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(&record.key_hash)
            .bind(record.principal_id.as_str())
            .bind(&record.request_fingerprint)
            .bind(&record.operation)
            .bind(&record.scope)
            .bind(&record.resource_id)
            .bind(&record.exclusive_scope)
            .bind(&record.attempt_id)
            .bind(&record.owner_instance_id)
            .bind(base_revision)
            .bind(record.response_status.map(i32::from))
            .bind(&record.response_body)
            .bind(&record.lease_expires_at)
            .bind(&record.created_at)
            .bind(i64::try_from(record.ttl_seconds).map_err(|_| {
                IronCrewError::Validation("Idempotency TTL is out of PostgreSQL range".into())
            })?)
            .execute(&mut *tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL idempotency claim insert failed: {error}"
                ))
            })?;
        tx.commit().await.map_err(|error| {
            IronCrewError::Validation(format!(
                "PostgreSQL idempotency claim commit failed: {error}"
            ))
        })?;
        Ok(IdempotencyClaimOutcome::Claimed(record))
    }

    async fn heartbeat_idempotency(
        &self,
        key_hash: &str,
        attempt_id: &str,
        new_lease_expires_at: &str,
    ) -> Result<bool> {
        validate_digest("idempotency key hash", key_hash)?;
        if attempt_id.is_empty() || attempt_id.len() > 128 {
            return Err(IronCrewError::Validation(
                "Idempotency attempt id must be 1..=128 bytes".into(),
            ));
        }
        parse_timestamp("idempotency heartbeat lease expiry", new_lease_expires_at)?;
        let mut tx = self.pool.begin().await.map_err(|error| {
            IronCrewError::Validation(format!(
                "PostgreSQL idempotency heartbeat transaction failed: {error}"
            ))
        })?;
        self.lock_idempotency_key(&mut tx, key_hash).await?;
        let (database_now, database_deadline) = self
            .database_clock_with_deadline(
                &mut tx,
                self.lease.ttl().as_secs(),
                "idempotency heartbeat",
            )
            .await?;
        let Some(record) = self
            .get_idempotency_in_transaction(&mut tx, key_hash)
            .await?
        else {
            return Ok(false);
        };
        if record.attempt_id != attempt_id {
            return Err(IronCrewError::Conflict(
                "Idempotency attempt changed before heartbeat".into(),
            ));
        }
        if !record.state.is_in_flight() {
            return Ok(record.state == IdempotencyState::Completed);
        }
        let sql = format!(
            "UPDATE {} SET lease_expires_at = $1, updated_at = $2 \
             WHERE key_hash = $3 AND attempt_id = $4 \
               AND state IN ('claimed', 'running')",
            self.idempotency_table
        );
        let result = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(&database_deadline)
            .bind(&database_now)
            .bind(key_hash)
            .bind(attempt_id)
            .execute(&mut *tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL idempotency heartbeat failed: {error}"
                ))
            })?;
        tx.commit().await.map_err(|error| {
            IronCrewError::Validation(format!(
                "PostgreSQL idempotency heartbeat commit failed: {error}"
            ))
        })?;
        Ok(result.rows_affected() == 1)
    }

    async fn heartbeat_idempotent_run(
        &self,
        run_id: &str,
        key_hash: &str,
        attempt_id: &str,
        new_lease_expires_at: &str,
    ) -> Result<RunFenceHeartbeat> {
        validate_digest("idempotency key hash", key_hash)?;
        if run_id.is_empty() || run_id.len() > 128 {
            return Err(IronCrewError::Validation(
                "Idempotent run id must be 1..=128 bytes".into(),
            ));
        }
        if attempt_id.is_empty() || attempt_id.len() > 128 {
            return Err(IronCrewError::Validation(
                "Idempotency attempt id must be 1..=128 bytes".into(),
            ));
        }
        // The absolute caller deadline remains part of the shared backend
        // contract, but PostgreSQL uses only its own clock for lease ordering.
        parse_timestamp(
            "idempotent run heartbeat lease expiry",
            new_lease_expires_at,
        )?;
        let mut tx = self.pool.begin().await.map_err(|error| {
            IronCrewError::Validation(format!(
                "PostgreSQL idempotent run heartbeat transaction failed: {error}"
            ))
        })?;
        self.lock_run_fence(&mut tx, true).await?;
        self.lock_resource(&mut tx, RUN_OPERATION, "", run_id)
            .await?;
        self.lock_idempotency_key(&mut tx, key_hash).await?;
        let (database_now, database_deadline) = self
            .database_clock_with_deadline(
                &mut tx,
                self.lease.ttl().as_secs(),
                "idempotent run heartbeat",
            )
            .await?;
        let Some(record) = self
            .get_idempotency_in_transaction(&mut tx, key_hash)
            .await?
        else {
            tx.commit().await.map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL missing run fence heartbeat commit failed: {error}"
                ))
            })?;
            return Ok(RunFenceHeartbeat::Lost);
        };
        if record.attempt_id != attempt_id {
            return Err(IronCrewError::Conflict(
                "Idempotency attempt changed before run heartbeat".into(),
            ));
        }
        if record.operation != RUN_OPERATION
            || record.resource_id != run_id
            || record.owner_instance_id != self.lease.instance_id()
        {
            tx.commit().await.map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL mismatched run fence heartbeat commit failed: {error}"
                ))
            })?;
            return Ok(RunFenceHeartbeat::Lost);
        }

        let run_sql = format!(
            "SELECT status, owner_instance_id, flow FROM {} WHERE run_id = $1 FOR UPDATE",
            self.table_name
        );
        let run = sqlx::query(sqlx::AssertSqlSafe(run_sql))
            .bind(run_id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL idempotent heartbeat run lookup failed: {error}"
                ))
            })?;

        let Some(run) = run else {
            if record.state != IdempotencyState::Claimed {
                tx.commit().await.map_err(|error| {
                    IronCrewError::Validation(format!(
                        "PostgreSQL lost run fence heartbeat commit failed: {error}"
                    ))
                })?;
                return Ok(RunFenceHeartbeat::Lost);
            }
            let ledger_sql = format!(
                "UPDATE {} SET lease_expires_at = $1, updated_at = $2 \
                 WHERE key_hash = $3 AND operation = $4 AND resource_id = $5 \
                   AND attempt_id = $6 AND owner_instance_id = $7 \
                   AND state = 'claimed'",
                self.idempotency_table
            );
            let renewed = sqlx::query(sqlx::AssertSqlSafe(ledger_sql))
                .bind(&database_deadline)
                .bind(&database_now)
                .bind(key_hash)
                .bind(RUN_OPERATION)
                .bind(run_id)
                .bind(attempt_id)
                .bind(self.lease.instance_id())
                .execute(&mut *tx)
                .await
                .map_err(|error| {
                    IronCrewError::Validation(format!(
                        "PostgreSQL claimed run fence heartbeat failed: {error}"
                    ))
                })?;
            let outcome = if renewed.rows_affected() == 1 {
                RunFenceHeartbeat::Owned
            } else {
                RunFenceHeartbeat::Lost
            };
            tx.commit().await.map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL claimed run fence heartbeat commit failed: {error}"
                ))
            })?;
            return Ok(outcome);
        };

        let run_owner: String = run
            .try_get("owner_instance_id")
            .map_err(|error| IronCrewError::Validation(format!("Column error: {error}")))?;
        let run_flow: String = run
            .try_get("flow")
            .map_err(|error| IronCrewError::Validation(format!("Column error: {error}")))?;
        if run_owner != self.lease.instance_id() || run_flow != record.scope {
            tx.commit().await.map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL unowned run fence heartbeat commit failed: {error}"
                ))
            })?;
            return Ok(RunFenceHeartbeat::Lost);
        }
        let status = run
            .try_get::<String, _>("status")
            .map_err(|error| IronCrewError::Validation(format!("Column error: {error}")))?
            .parse::<RunStatus>()?;
        if status.is_terminal() {
            tx.commit().await.map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL terminal run fence heartbeat commit failed: {error}"
                ))
            })?;
            return Ok(RunFenceHeartbeat::Terminal(status));
        }
        if record.state != IdempotencyState::Running || !status.is_in_flight() {
            tx.commit().await.map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL lost run fence heartbeat commit failed: {error}"
                ))
            })?;
            return Ok(RunFenceHeartbeat::Lost);
        }

        let run_update_sql = format!(
            "UPDATE {} SET lease_expires_at = $1 \
             WHERE run_id = $2 AND owner_instance_id = $3 \
               AND status IN ('running', 'waiting_for_input')",
            self.table_name
        );
        let run_renewed = sqlx::query(sqlx::AssertSqlSafe(run_update_sql))
            .bind(&database_deadline)
            .bind(run_id)
            .bind(self.lease.instance_id())
            .execute(&mut *tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL idempotent run lease heartbeat failed: {error}"
                ))
            })?;
        let ledger_update_sql = format!(
            "UPDATE {} SET lease_expires_at = $1, updated_at = $2 \
             WHERE key_hash = $3 AND operation = $4 AND resource_id = $5 \
               AND attempt_id = $6 AND owner_instance_id = $7 \
               AND state = 'running'",
            self.idempotency_table
        );
        let ledger_renewed = sqlx::query(sqlx::AssertSqlSafe(ledger_update_sql))
            .bind(&database_deadline)
            .bind(&database_now)
            .bind(key_hash)
            .bind(RUN_OPERATION)
            .bind(run_id)
            .bind(attempt_id)
            .bind(self.lease.instance_id())
            .execute(&mut *tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL idempotent run ledger heartbeat failed: {error}"
                ))
            })?;
        if run_renewed.rows_affected() != 1 || ledger_renewed.rows_affected() != 1 {
            tx.rollback().await.map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL lost run fence heartbeat rollback failed: {error}"
                ))
            })?;
            return Ok(RunFenceHeartbeat::Lost);
        }
        tx.commit().await.map_err(|error| {
            IronCrewError::Validation(format!(
                "PostgreSQL idempotent run heartbeat commit failed: {error}"
            ))
        })?;
        Ok(RunFenceHeartbeat::Owned)
    }

    async fn complete_idempotency_with_limits(
        &self,
        completion: IdempotencyCompletion,
        limits: IdempotencyLimits,
    ) -> Result<IdempotencyCompletionOutcome> {
        completion.validate()?;
        limits.validate()?;
        let mut tx = self.pool.begin().await.map_err(|error| {
            IronCrewError::Validation(format!(
                "PostgreSQL idempotency completion transaction failed: {error}"
            ))
        })?;
        self.lock_idempotency_quota(&mut tx).await?;
        self.lock_idempotency_principal(&mut tx, &completion.principal_id)
            .await?;
        self.lock_idempotency_key(&mut tx, &completion.key_hash)
            .await?;
        let record = self
            .get_idempotency_in_transaction(&mut tx, &completion.key_hash)
            .await?
            .ok_or_else(|| {
                IronCrewError::Validation("Idempotency claim not found during completion".into())
            })?;
        if record.principal_id != completion.principal_id
            || record.request_fingerprint != completion.request_fingerprint
            || record.attempt_id != completion.attempt_id
            || record.owner_instance_id != completion.owner_instance_id
        {
            return Err(IronCrewError::Conflict(format!(
                "Idempotency claim '{}' is fenced by a different attempt",
                completion.key_hash
            )));
        }
        if record.state == IdempotencyState::Completed {
            let outcome = IdempotencyCompletionOutcome {
                replayable: record.replayable(),
                already_completed: true,
            };
            tx.commit().await.map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL idempotency completion commit failed: {error}"
                ))
            })?;
            return Ok(outcome);
        }
        if record.state == IdempotencyState::Indeterminate {
            return Err(IronCrewError::Conflict(
                "Indeterminate idempotency outcomes cannot be completed".into(),
            ));
        }
        let (database_completed_at, database_expires_at) = self
            .database_clock_with_deadline(&mut tx, record.ttl_seconds, "idempotency completion")
            .await?;

        let (global_usage, principal_usage) = self
            .idempotency_accounting_for_update(&mut tx, &completion.principal_id)
            .await?;
        let old_response_bytes = record.response_body.as_ref().map_or(0, String::len);
        let global_without_record = global_usage
            .response_bytes
            .checked_sub(old_response_bytes)
            .ok_or_else(|| {
                IronCrewError::Validation(
                    "PostgreSQL global idempotency response accounting is inconsistent".into(),
                )
            })?;
        let principal_without_record = principal_usage
            .response_bytes
            .checked_sub(old_response_bytes)
            .ok_or_else(|| {
                IronCrewError::Validation(
                    "PostgreSQL principal idempotency response accounting is inconsistent".into(),
                )
            })?;
        let response_body = completion.response_body.as_ref().filter(|body| {
            global_without_record
                .checked_add(body.len())
                .is_some_and(|total| total <= limits.global_max_response_bytes)
                && principal_without_record
                    .checked_add(body.len())
                    .is_some_and(|total| total <= limits.principal_max_response_bytes)
        });
        let sql = format!(
            "UPDATE {} SET state = 'completed', response_status = $1, \
             response_body = $2, lease_expires_at = '', updated_at = $3, \
             completed_at = $3, expires_at = $4 \
             WHERE key_hash = $5 AND request_fingerprint = $6 \
               AND attempt_id = $7 AND owner_instance_id = $8 \
               AND state IN ('claimed', 'running')",
            self.idempotency_table
        );
        let result = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(i32::from(completion.response_status))
            .bind(response_body)
            .bind(&database_completed_at)
            .bind(&database_expires_at)
            .bind(&completion.key_hash)
            .bind(&completion.request_fingerprint)
            .bind(&completion.attempt_id)
            .bind(&completion.owner_instance_id)
            .execute(&mut *tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL idempotency completion update failed: {error}"
                ))
            })?;
        if result.rows_affected() != 1 {
            return Err(IronCrewError::Conflict(format!(
                "Idempotency claim '{}' changed before completion",
                completion.key_hash
            )));
        }
        tx.commit().await.map_err(|error| {
            IronCrewError::Validation(format!(
                "PostgreSQL idempotency completion commit failed: {error}"
            ))
        })?;
        Ok(IdempotencyCompletionOutcome {
            replayable: response_body.is_some(),
            already_completed: false,
        })
    }

    async fn commit_conversation_idempotency_with_limits(
        &self,
        completion: IdempotencyCompletion,
        conversation: &ConversationRecord,
        limits: IdempotencyLimits,
    ) -> Result<ConversationIdempotencyCommit> {
        completion.validate()?;
        limits.validate()?;
        let messages_json = serde_json::to_string(&conversation.messages).map_err(|error| {
            IronCrewError::Validation(format!(
                "Failed to serialize idempotent conversation messages: {error}"
            ))
        })?;
        let expected_revision = i64::try_from(conversation.revision).map_err(|_| {
            IronCrewError::Validation("Conversation revision is out of range".into())
        })?;
        let mut tx = self.pool.begin().await.map_err(|error| {
            IronCrewError::Validation(format!(
                "PostgreSQL idempotent conversation transaction failed: {error}"
            ))
        })?;
        self.lock_idempotency_quota(&mut tx).await?;
        self.lock_idempotency_principal(&mut tx, &completion.principal_id)
            .await?;
        self.lock_resource(
            &mut tx,
            CONVERSATION_MESSAGE_OPERATION,
            conversation.flow_path.as_deref().unwrap_or(""),
            &conversation.id,
        )
        .await?;
        self.lock_idempotency_key(&mut tx, &completion.key_hash)
            .await?;
        let record = self
            .get_idempotency_in_transaction(&mut tx, &completion.key_hash)
            .await?
            .ok_or_else(|| {
                IronCrewError::Validation(
                    "Idempotency claim not found during conversation commit".into(),
                )
            })?;
        if record.principal_id != completion.principal_id
            || record.request_fingerprint != completion.request_fingerprint
            || record.attempt_id != completion.attempt_id
            || record.owner_instance_id != completion.owner_instance_id
        {
            return Err(IronCrewError::Conflict(format!(
                "Idempotency claim '{}' is fenced by a different attempt",
                completion.key_hash
            )));
        }
        if record.operation != CONVERSATION_MESSAGE_OPERATION
            || record.resource_id != conversation.id
            || record.scope != conversation.flow_path.as_deref().unwrap_or("")
        {
            return Err(IronCrewError::Conflict(format!(
                "Idempotency claim '{}' does not match the conversation scope",
                completion.key_hash
            )));
        }
        let base_revision = record.base_revision.ok_or_else(|| {
            IronCrewError::Validation("Conversation idempotency claim has no base revision".into())
        })?;
        if base_revision != conversation.revision {
            return Err(IronCrewError::Conflict(format!(
                "Conversation '{}' changed before idempotent commit",
                conversation.id
            )));
        }
        if record.state == IdempotencyState::Completed {
            tx.commit().await.map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL completed conversation commit failed: {error}"
                ))
            })?;
            return Ok(ConversationIdempotencyCommit {
                revision: base_revision.saturating_add(1),
                replayable: record.replayable(),
                already_completed: true,
            });
        }
        if record.state == IdempotencyState::Indeterminate {
            return Err(IronCrewError::Conflict(
                "Indeterminate conversation outcomes cannot be committed".into(),
            ));
        }
        let (database_completed_at, database_expires_at) = self
            .database_clock_with_deadline(
                &mut tx,
                record.ttl_seconds,
                "conversation idempotency completion",
            )
            .await?;

        let select_sql = format!(
            "SELECT revision FROM {} \
             WHERE id = $1 AND flow_path IS NOT DISTINCT FROM $2 FOR UPDATE",
            self.conversations_table
        );
        let current: Option<i64> = sqlx::query_scalar(sqlx::AssertSqlSafe(select_sql))
            .bind(&conversation.id)
            .bind(&conversation.flow_path)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL idempotent conversation revision read failed: {error}"
                ))
            })?;
        let revision: Option<i64> = match current {
            None if expected_revision == 0 => {
                let insert_sql = format!(
                    "INSERT INTO {} \
                     (id, flow_name, flow_path, agent_name, messages, created_at, updated_at, revision) \
                     VALUES ($1, $2, $3, $4, $5::jsonb, $6, $7, 1) \
                     ON CONFLICT (flow_path, id) DO NOTHING RETURNING revision",
                    self.conversations_table
                );
                sqlx::query_scalar(sqlx::AssertSqlSafe(insert_sql))
                    .bind(&conversation.id)
                    .bind(&conversation.flow_name)
                    .bind(&conversation.flow_path)
                    .bind(&conversation.agent_name)
                    .bind(&messages_json)
                    .bind(&conversation.created_at)
                    .bind(&conversation.updated_at)
                    .fetch_optional(&mut *tx)
                    .await
                    .map_err(|error| {
                        IronCrewError::Validation(format!(
                            "PostgreSQL idempotent conversation insert failed: {error}"
                        ))
                    })?
            }
            Some(current) if current == expected_revision => {
                let update_sql = format!(
                    "UPDATE {} SET flow_name = $3, agent_name = $4, \
                     messages = $5::jsonb, created_at = $6, updated_at = $7, \
                     revision = revision + 1 \
                     WHERE id = $1 AND flow_path IS NOT DISTINCT FROM $2 AND revision = $8 \
                     RETURNING revision",
                    self.conversations_table
                );
                sqlx::query_scalar(sqlx::AssertSqlSafe(update_sql))
                    .bind(&conversation.id)
                    .bind(&conversation.flow_path)
                    .bind(&conversation.flow_name)
                    .bind(&conversation.agent_name)
                    .bind(&messages_json)
                    .bind(&conversation.created_at)
                    .bind(&conversation.updated_at)
                    .bind(expected_revision)
                    .fetch_optional(&mut *tx)
                    .await
                    .map_err(|error| {
                        IronCrewError::Validation(format!(
                            "PostgreSQL idempotent conversation update failed: {error}"
                        ))
                    })?
            }
            _ => None,
        };
        let revision = revision.ok_or_else(|| {
            IronCrewError::Conflict(format!(
                "Conversation '{}' changed since revision {}; reopen it before saving",
                conversation.id, conversation.revision
            ))
        })?;

        let (global_usage, principal_usage) = self
            .idempotency_accounting_for_update(&mut tx, &completion.principal_id)
            .await?;
        let old_response_bytes = record.response_body.as_ref().map_or(0, String::len);
        let global_without_record = global_usage
            .response_bytes
            .checked_sub(old_response_bytes)
            .ok_or_else(|| {
                IronCrewError::Validation(
                    "PostgreSQL global idempotency response accounting is inconsistent".into(),
                )
            })?;
        let principal_without_record = principal_usage
            .response_bytes
            .checked_sub(old_response_bytes)
            .ok_or_else(|| {
                IronCrewError::Validation(
                    "PostgreSQL principal idempotency response accounting is inconsistent".into(),
                )
            })?;
        let response_body = completion.response_body.as_ref().filter(|body| {
            global_without_record
                .checked_add(body.len())
                .is_some_and(|total| total <= limits.global_max_response_bytes)
                && principal_without_record
                    .checked_add(body.len())
                    .is_some_and(|total| total <= limits.principal_max_response_bytes)
        });
        let update_idempotency = format!(
            "UPDATE {} SET state = 'completed', response_status = $1, \
             response_body = $2, lease_expires_at = '', updated_at = $3, \
             completed_at = $3, expires_at = $4 \
             WHERE key_hash = $5 AND request_fingerprint = $6 \
               AND attempt_id = $7 AND owner_instance_id = $8 \
               AND state IN ('claimed', 'running')",
            self.idempotency_table
        );
        let updated = sqlx::query(sqlx::AssertSqlSafe(update_idempotency))
            .bind(i32::from(completion.response_status))
            .bind(response_body)
            .bind(&database_completed_at)
            .bind(&database_expires_at)
            .bind(&completion.key_hash)
            .bind(&completion.request_fingerprint)
            .bind(&completion.attempt_id)
            .bind(&completion.owner_instance_id)
            .execute(&mut *tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL idempotent conversation completion failed: {error}"
                ))
            })?;
        if updated.rows_affected() != 1 {
            return Err(IronCrewError::Conflict(format!(
                "Idempotency claim '{}' changed before conversation commit",
                completion.key_hash
            )));
        }
        tx.commit().await.map_err(|error| {
            IronCrewError::Validation(format!(
                "PostgreSQL idempotent conversation commit failed: {error}"
            ))
        })?;
        Ok(ConversationIdempotencyCommit {
            revision: u64::try_from(revision)
                .map_err(|_| IronCrewError::Validation("Invalid conversation revision".into()))?,
            replayable: response_body.is_some(),
            already_completed: false,
        })
    }

    async fn mark_idempotency_indeterminate(
        &self,
        key_hash: &str,
        attempt_id: &str,
        completed_at: &str,
        expires_at: &str,
    ) -> Result<bool> {
        validate_digest("idempotency key hash", key_hash)?;
        parse_timestamp("idempotency completion time", completed_at)?;
        parse_timestamp("idempotency retention expiry", expires_at)?;
        let Some(principal_id) = self.idempotency_principal_for_key(key_hash).await? else {
            return Ok(false);
        };
        let mut tx = self.pool.begin().await.map_err(|error| {
            IronCrewError::Validation(format!(
                "PostgreSQL idempotency indeterminate transaction failed: {error}"
            ))
        })?;
        self.lock_idempotency_quota(&mut tx).await?;
        self.lock_idempotency_principal(&mut tx, &principal_id)
            .await?;
        self.lock_idempotency_key(&mut tx, key_hash).await?;
        let Some(record) = self
            .get_idempotency_in_transaction(&mut tx, key_hash)
            .await?
        else {
            return Ok(false);
        };
        if record.principal_id != principal_id {
            return Err(IronCrewError::Conflict(
                "Idempotency principal changed before indeterminate transition".into(),
            ));
        }
        if record.attempt_id != attempt_id {
            return Err(IronCrewError::Conflict(
                "Idempotency attempt changed before indeterminate transition".into(),
            ));
        }
        if record.state.is_terminal() {
            return Ok(false);
        }
        let (database_completed_at, database_expires_at) = self
            .database_clock_with_deadline(
                &mut tx,
                record.ttl_seconds,
                "idempotency indeterminate completion",
            )
            .await?;
        let sql = format!(
            "UPDATE {} SET state = 'indeterminate', response_status = NULL, \
             response_body = NULL, lease_expires_at = '', updated_at = $1, \
             completed_at = $1, expires_at = $2 \
             WHERE key_hash = $3 AND attempt_id = $4 \
               AND state IN ('claimed', 'running')",
            self.idempotency_table
        );
        let result = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(&database_completed_at)
            .bind(&database_expires_at)
            .bind(key_hash)
            .bind(attempt_id)
            .execute(&mut *tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL idempotency indeterminate update failed: {error}"
                ))
            })?;
        tx.commit().await.map_err(|error| {
            IronCrewError::Validation(format!(
                "PostgreSQL idempotency indeterminate commit failed: {error}"
            ))
        })?;
        Ok(result.rows_affected() == 1)
    }

    async fn release_idempotency(&self, key_hash: &str, attempt_id: &str) -> Result<bool> {
        validate_digest("idempotency key hash", key_hash)?;
        let Some(principal_id) = self.idempotency_principal_for_key(key_hash).await? else {
            return Ok(false);
        };
        let mut tx = self.pool.begin().await.map_err(|error| {
            IronCrewError::Validation(format!(
                "PostgreSQL idempotency release transaction failed: {error}"
            ))
        })?;
        self.lock_idempotency_quota(&mut tx).await?;
        self.lock_idempotency_principal(&mut tx, &principal_id)
            .await?;
        self.lock_idempotency_key(&mut tx, key_hash).await?;
        let Some(record) = self
            .get_idempotency_in_transaction(&mut tx, key_hash)
            .await?
        else {
            return Ok(false);
        };
        if record.principal_id != principal_id {
            return Err(IronCrewError::Conflict(
                "Idempotency principal changed before release".into(),
            ));
        }
        if record.attempt_id != attempt_id {
            return Err(IronCrewError::Conflict(
                "Idempotency attempt changed before release".into(),
            ));
        }
        if !record.state.is_in_flight() {
            return Ok(false);
        }
        let sql = format!(
            "DELETE FROM {} WHERE key_hash = $1 AND attempt_id = $2 \
             AND state IN ('claimed', 'running')",
            self.idempotency_table
        );
        let result = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(key_hash)
            .bind(attempt_id)
            .execute(&mut *tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!("PostgreSQL idempotency release failed: {error}"))
            })?;
        tx.commit().await.map_err(|error| {
            IronCrewError::Validation(format!(
                "PostgreSQL idempotency release commit failed: {error}"
            ))
        })?;
        Ok(result.rows_affected() == 1)
    }

    async fn prune_idempotency(&self, now: &str, limit: usize) -> Result<usize> {
        parse_timestamp("idempotency prune time", now)?;
        if limit == 0 {
            return Ok(0);
        }
        let mut tx = self.pool.begin().await.map_err(|error| {
            IronCrewError::Validation(format!(
                "PostgreSQL idempotency prune transaction failed: {error}"
            ))
        })?;
        self.lock_idempotency_quota(&mut tx).await?;
        let (database_now, _) = self
            .database_clock_with_deadline(&mut tx, 0, "idempotency pruning")
            .await?;
        let sql = format!(
            "DELETE FROM {table} WHERE key_hash IN (\
                 SELECT key_hash FROM {table} \
                 WHERE state IN ('completed', 'indeterminate') \
                   AND expires_at IS NOT NULL \
                   AND expires_at::timestamptz <= $1::timestamptz \
                 ORDER BY expires_at::timestamptz, key_hash LIMIT $2\
             )",
            table = self.idempotency_table
        );
        let result = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(&database_now)
            .bind(i64::try_from(limit).unwrap_or(i64::MAX))
            .execute(&mut *tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!("PostgreSQL idempotency prune failed: {error}"))
            })?;
        tx.commit().await.map_err(|error| {
            IronCrewError::Validation(format!(
                "PostgreSQL idempotency prune commit failed: {error}"
            ))
        })?;
        Ok(result.rows_affected() as usize)
    }

    async fn idempotency_usage(
        &self,
        principal_id: &PrincipalId,
        limits: IdempotencyLimits,
    ) -> Result<IdempotencyUsage> {
        principal_id.validate()?;
        limits.validate()?;
        let threshold = |limit: usize, percentage: usize| {
            i64::try_from(limit.saturating_mul(percentage).div_ceil(100).max(1)).unwrap_or(i64::MAX)
        };
        let sql = format!(
            "SELECT \
                 global.record_count AS global_records, \
                 global.in_flight_count AS global_in_flight, \
                 global.response_bytes AS global_response_bytes, \
                 COALESCE(principal.record_count, 0) AS principal_records, \
                 COALESCE(principal.in_flight_count, 0) AS principal_in_flight, \
                 COALESCE(principal.response_bytes, 0) AS principal_response_bytes, \
                 stats.principal_count, stats.max_principal_records, \
                 stats.max_principal_in_flight, stats.max_principal_response_bytes, \
                 stats.at_80, stats.at_90, stats.at_100 \
             FROM {accounting} AS global \
             LEFT JOIN {accounting} AS principal \
               ON principal.principal_id = $1 AND principal.is_global = FALSE \
             CROSS JOIN LATERAL (\
                 SELECT COUNT(*)::BIGINT AS principal_count, \
                        COALESCE(MAX(record_count), 0)::BIGINT AS max_principal_records, \
                        COALESCE(MAX(in_flight_count), 0)::BIGINT AS max_principal_in_flight, \
                        COALESCE(MAX(response_bytes), 0)::BIGINT AS max_principal_response_bytes, \
                        COUNT(*) FILTER (WHERE record_count >= $2 OR in_flight_count >= $3 \
                                                OR response_bytes >= $4)::BIGINT AS at_80, \
                        COUNT(*) FILTER (WHERE record_count >= $5 OR in_flight_count >= $6 \
                                                OR response_bytes >= $7)::BIGINT AS at_90, \
                        COUNT(*) FILTER (WHERE record_count >= $8 OR in_flight_count >= $9 \
                                                OR response_bytes >= $10)::BIGINT AS at_100 \
                 FROM {accounting} WHERE is_global = FALSE\
             ) AS stats \
             WHERE global.principal_id = 'global' AND global.is_global = TRUE",
            accounting = self.idempotency_accounting_table
        );
        let row = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(principal_id.as_str())
            .bind(threshold(limits.principal_max_records, 80))
            .bind(threshold(limits.principal_max_in_flight, 80))
            .bind(threshold(limits.principal_max_response_bytes, 80))
            .bind(threshold(limits.principal_max_records, 90))
            .bind(threshold(limits.principal_max_in_flight, 90))
            .bind(threshold(limits.principal_max_response_bytes, 90))
            .bind(threshold(limits.principal_max_records, 100))
            .bind(threshold(limits.principal_max_in_flight, 100))
            .bind(threshold(limits.principal_max_response_bytes, 100))
            .fetch_optional(&self.pool)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL idempotency usage query failed: {error}"
                ))
            })?
            .ok_or_else(|| {
                IronCrewError::Validation(
                    "PostgreSQL global idempotency accounting row is missing".into(),
                )
            })?;
        Ok(IdempotencyUsage {
            global_records: accounting_row_value(&row, "global_records")?,
            global_in_flight: accounting_row_value(&row, "global_in_flight")?,
            global_response_bytes: accounting_row_value(&row, "global_response_bytes")?,
            principal_records: accounting_row_value(&row, "principal_records")?,
            principal_in_flight: accounting_row_value(&row, "principal_in_flight")?,
            principal_response_bytes: accounting_row_value(&row, "principal_response_bytes")?,
            principal_count: accounting_row_value(&row, "principal_count")?,
            max_principal_records: accounting_row_value(&row, "max_principal_records")?,
            max_principal_in_flight: accounting_row_value(&row, "max_principal_in_flight")?,
            max_principal_response_bytes: accounting_row_value(
                &row,
                "max_principal_response_bytes",
            )?,
            principals_at_or_above_80_percent: accounting_row_value(&row, "at_80")?,
            principals_at_or_above_90_percent: accounting_row_value(&row, "at_90")?,
            principals_at_or_above_100_percent: accounting_row_value(&row, "at_100")?,
        })
    }

    // ─── Persistent sessions ────────────────────────────────────────────────

    async fn save_conversation(&self, record: &ConversationRecord) -> Result<u64> {
        let messages_json = serde_json::to_string(&record.messages).map_err(|e| {
            IronCrewError::Validation(format!("Failed to serialize messages: {}", e))
        })?;
        let expected_revision = i64::try_from(record.revision).map_err(|_| {
            IronCrewError::Validation("Conversation revision is out of range".into())
        })?;
        let mut tx = self.pool.begin().await.map_err(|e| {
            IronCrewError::Validation(format!(
                "PostgreSQL save_conversation transaction error: {e}"
            ))
        })?;
        self.lock_resource(
            &mut tx,
            CONVERSATION_MESSAGE_OPERATION,
            record.flow_path.as_deref().unwrap_or(""),
            &record.id,
        )
        .await?;
        let guard_sql = format!(
            "SELECT EXISTS (SELECT 1 FROM {} \
             WHERE operation = $1 AND scope = $2 AND resource_id = $3 \
               AND state IN ('claimed', 'running'))",
            self.idempotency_table
        );
        let active: bool = sqlx::query_scalar(sqlx::AssertSqlSafe(guard_sql))
            .bind(CONVERSATION_MESSAGE_OPERATION)
            .bind(record.flow_path.as_deref().unwrap_or(""))
            .bind(&record.id)
            .fetch_one(&mut *tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL conversation idempotency guard failed: {error}"
                ))
            })?;
        if active {
            return Err(IronCrewError::Conflict(format!(
                "Conversation '{}' has an active idempotent message operation",
                record.id
            )));
        }
        let select_sql = format!(
            "SELECT revision FROM {} \
             WHERE id = $1 AND flow_path IS NOT DISTINCT FROM $2 FOR UPDATE",
            self.conversations_table
        );
        let current: Option<i64> = sqlx::query_scalar(sqlx::AssertSqlSafe(select_sql))
            .bind(&record.id)
            .bind(&record.flow_path)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|e| {
                IronCrewError::Validation(format!(
                    "PostgreSQL save_conversation revision read error: {e}"
                ))
            })?;
        let revision: Option<i64> = match current {
            None if expected_revision == 0 => {
                let insert_sql = format!(
                    "INSERT INTO {} \
                     (id, flow_name, flow_path, agent_name, messages, created_at, updated_at, revision) \
                     VALUES ($1, $2, $3, $4, $5::jsonb, $6, $7, 1) \
                     ON CONFLICT (flow_path, id) DO NOTHING RETURNING revision",
                    self.conversations_table
                );
                sqlx::query_scalar(sqlx::AssertSqlSafe(insert_sql))
                    .bind(&record.id)
                    .bind(&record.flow_name)
                    .bind(&record.flow_path)
                    .bind(&record.agent_name)
                    .bind(&messages_json)
                    .bind(&record.created_at)
                    .bind(&record.updated_at)
                    .fetch_optional(&mut *tx)
                    .await
                    .map_err(|e| {
                        IronCrewError::Validation(format!(
                            "PostgreSQL save_conversation insert error: {e}"
                        ))
                    })?
            }
            Some(current) if current == expected_revision => {
                let update_sql = format!(
                    "UPDATE {} SET flow_name = $3, agent_name = $4, \
                     messages = $5::jsonb, created_at = $6, updated_at = $7, \
                     revision = revision + 1 \
                     WHERE id = $1 AND flow_path IS NOT DISTINCT FROM $2 AND revision = $8 \
                     RETURNING revision",
                    self.conversations_table
                );
                sqlx::query_scalar(sqlx::AssertSqlSafe(update_sql))
                    .bind(&record.id)
                    .bind(&record.flow_path)
                    .bind(&record.flow_name)
                    .bind(&record.agent_name)
                    .bind(&messages_json)
                    .bind(&record.created_at)
                    .bind(&record.updated_at)
                    .bind(expected_revision)
                    .fetch_optional(&mut *tx)
                    .await
                    .map_err(|e| {
                        IronCrewError::Validation(format!(
                            "PostgreSQL save_conversation update error: {e}"
                        ))
                    })?
            }
            _ => None,
        };
        let revision = revision.ok_or_else(|| {
            IronCrewError::Conflict(format!(
                "Conversation '{}' changed since revision {}; reopen it before saving",
                record.id, record.revision
            ))
        })?;
        tx.commit().await.map_err(|e| {
            IronCrewError::Validation(format!("PostgreSQL save_conversation commit error: {e}"))
        })?;
        u64::try_from(revision)
            .map_err(|_| IronCrewError::Validation("Invalid conversation revision".into()))
    }

    async fn get_conversation(
        &self,
        flow_path: Option<&str>,
        id: &str,
    ) -> Result<Option<ConversationRecord>> {
        // Flow-scoped lookup: when `flow_path` is Some, require an exact
        // match. `$2::TEXT IS NULL` lets the same query serve global
        // (unscoped) admin lookups.
        let sql = format!(
            "SELECT id, flow_name, flow_path, agent_name, messages::text, created_at, updated_at, revision \
             FROM {} WHERE id = $1 AND ($2::TEXT IS NULL OR flow_path = $2)",
            self.conversations_table
        );
        let row_opt = sqlx::query(sqlx::AssertSqlSafe(sql.to_string()))
            .bind(id)
            .bind(flow_path)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| {
                IronCrewError::Validation(format!("PostgreSQL get_conversation error: {}", e))
            })?;
        let Some(row) = row_opt else {
            return Ok(None);
        };
        let messages_str: String = row
            .try_get("messages")
            .map_err(|e| IronCrewError::Validation(format!("Column error: {}", e)))?;
        Ok(Some(ConversationRecord {
            id: row
                .try_get("id")
                .map_err(|e| IronCrewError::Validation(e.to_string()))?,
            flow_name: row
                .try_get("flow_name")
                .map_err(|e| IronCrewError::Validation(e.to_string()))?,
            flow_path: row
                .try_get("flow_path")
                .map_err(|e| IronCrewError::Validation(e.to_string()))?,
            agent_name: row
                .try_get("agent_name")
                .map_err(|e| IronCrewError::Validation(e.to_string()))?,
            messages: decode_stored_json(&messages_str, "conversations.messages")?,
            created_at: row
                .try_get("created_at")
                .map_err(|e| IronCrewError::Validation(e.to_string()))?,
            updated_at: row
                .try_get("updated_at")
                .map_err(|e| IronCrewError::Validation(e.to_string()))?,
            revision: u64::try_from(
                row.try_get::<i64, _>("revision")
                    .map_err(|e| IronCrewError::Validation(e.to_string()))?,
            )
            .map_err(|_| IronCrewError::Validation("Invalid conversation revision".into()))?,
        }))
    }

    async fn delete_conversation(&self, flow_path: Option<&str>, id: &str) -> Result<()> {
        let mut tx = self.pool.begin().await.map_err(|error| {
            IronCrewError::Validation(format!(
                "PostgreSQL delete_conversation transaction error: {error}"
            ))
        })?;
        self.lock_resource(
            &mut tx,
            CONVERSATION_MESSAGE_OPERATION,
            flow_path.unwrap_or(""),
            id,
        )
        .await?;
        let guard_sql = format!(
            "SELECT EXISTS (SELECT 1 FROM {} \
             WHERE operation = $1 AND scope = $2 AND resource_id = $3 \
               AND state IN ('claimed', 'running'))",
            self.idempotency_table
        );
        let active: bool = sqlx::query_scalar(sqlx::AssertSqlSafe(guard_sql))
            .bind(CONVERSATION_MESSAGE_OPERATION)
            .bind(flow_path.unwrap_or(""))
            .bind(id)
            .fetch_one(&mut *tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL conversation delete idempotency guard failed: {error}"
                ))
            })?;
        if active {
            return Err(IronCrewError::Conflict(format!(
                "Conversation '{id}' has an active idempotent message operation"
            )));
        }
        let sql = format!(
            "DELETE FROM {} WHERE id = $1 AND ($2::TEXT IS NULL OR flow_path = $2)",
            self.conversations_table
        );
        sqlx::query(sqlx::AssertSqlSafe(sql.to_string()))
            .bind(id)
            .bind(flow_path)
            .execute(&mut *tx)
            .await
            .map_err(|e| {
                IronCrewError::Validation(format!("PostgreSQL delete_conversation error: {}", e))
            })?;
        tx.commit().await.map_err(|error| {
            IronCrewError::Validation(format!(
                "PostgreSQL delete_conversation commit error: {error}"
            ))
        })?;
        Ok(())
    }

    async fn list_conversations(
        &self,
        flow_path: Option<&str>,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<ConversationSummary>> {
        let sql = format!(
            "SELECT c.id, c.flow_path, c.agent_name, \
                    (SELECT COUNT(*) FROM jsonb_array_elements(c.messages) AS message \
                     WHERE message->>'role' = 'user') AS turn_count, \
                    c.created_at, c.updated_at \
             FROM {} AS c \
             WHERE ($1::TEXT IS NULL OR c.flow_path = $1) \
             ORDER BY c.updated_at DESC \
             LIMIT $2 OFFSET $3",
            self.conversations_table
        );
        let limit_i = if limit == 0 { i64::MAX } else { limit as i64 };
        let rows = sqlx::query(sqlx::AssertSqlSafe(sql.to_string()))
            .bind(flow_path)
            .bind(limit_i)
            .bind(offset as i64)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| {
                IronCrewError::Validation(format!("PostgreSQL list_conversations error: {}", e))
            })?;
        let mut summaries = Vec::with_capacity(rows.len());
        for row in rows.iter() {
            let turn_count: i64 = row
                .try_get("turn_count")
                .map_err(|e| IronCrewError::Validation(format!("Column error: {}", e)))?;
            summaries.push(ConversationSummary {
                id: row
                    .try_get("id")
                    .map_err(|e| IronCrewError::Validation(e.to_string()))?,
                flow_path: row
                    .try_get("flow_path")
                    .map_err(|e| IronCrewError::Validation(e.to_string()))?,
                agent_name: row
                    .try_get("agent_name")
                    .map_err(|e| IronCrewError::Validation(e.to_string()))?,
                created_at: row
                    .try_get("created_at")
                    .map_err(|e| IronCrewError::Validation(e.to_string()))?,
                updated_at: row
                    .try_get("updated_at")
                    .map_err(|e| IronCrewError::Validation(e.to_string()))?,
                turn_count: usize::try_from(turn_count).map_err(|_| {
                    IronCrewError::Validation("PostgreSQL turn_count is out of range".into())
                })?,
            });
        }
        Ok(summaries)
    }

    async fn count_conversations(&self, flow_path: Option<&str>) -> Result<u64> {
        let sql = format!(
            "SELECT COUNT(*) FROM {} \
             WHERE ($1::TEXT IS NULL OR flow_path = $1)",
            self.conversations_table
        );
        let row = sqlx::query(sqlx::AssertSqlSafe(sql.to_string()))
            .bind(flow_path)
            .fetch_one(&self.pool)
            .await
            .map_err(|e| {
                IronCrewError::Validation(format!("PostgreSQL count_conversations error: {}", e))
            })?;
        let count: i64 = row
            .try_get(0)
            .map_err(|e| IronCrewError::Validation(format!("Column error: {}", e)))?;
        Ok(count as u64)
    }

    async fn save_dialog_state(&self, record: &DialogStateRecord) -> Result<u64> {
        let agents_json = serde_json::to_string(&record.agent_names).map_err(|e| {
            IronCrewError::Validation(format!("Failed to serialize agent_names: {}", e))
        })?;
        let transcript_json = serde_json::to_string(&record.transcript).map_err(|e| {
            IronCrewError::Validation(format!("Failed to serialize transcript: {}", e))
        })?;
        let expected_revision = i64::try_from(record.revision)
            .map_err(|_| IronCrewError::Validation("Dialog revision is out of range".into()))?;
        let mut tx = self.pool.begin().await.map_err(|e| {
            IronCrewError::Validation(format!(
                "PostgreSQL save_dialog_state transaction error: {e}"
            ))
        })?;
        let select_sql = format!(
            "SELECT revision FROM {} \
             WHERE id = $1 AND flow_path IS NOT DISTINCT FROM $2 FOR UPDATE",
            self.dialogs_table
        );
        let current: Option<i64> = sqlx::query_scalar(sqlx::AssertSqlSafe(select_sql))
            .bind(&record.id)
            .bind(&record.flow_path)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|e| {
                IronCrewError::Validation(format!(
                    "PostgreSQL save_dialog_state revision read error: {e}"
                ))
            })?;
        let revision: Option<i64> = match current {
            None if expected_revision == 0 => {
                let insert_sql = format!(
                    "INSERT INTO {} \
                     (id, flow_name, flow_path, agent_names, starter, transcript, next_index, stopped, stop_reason, created_at, updated_at, revision) \
                     VALUES ($1, $2, $3, $4::jsonb, $5, $6::jsonb, $7, $8, $9, $10, $11, 1) \
                     ON CONFLICT (flow_path, id) DO NOTHING RETURNING revision",
                    self.dialogs_table
                );
                sqlx::query_scalar(sqlx::AssertSqlSafe(insert_sql))
                    .bind(&record.id)
                    .bind(&record.flow_name)
                    .bind(&record.flow_path)
                    .bind(&agents_json)
                    .bind(&record.starter)
                    .bind(&transcript_json)
                    .bind(record.next_index as i32)
                    .bind(record.stopped)
                    .bind(&record.stop_reason)
                    .bind(&record.created_at)
                    .bind(&record.updated_at)
                    .fetch_optional(&mut *tx)
                    .await
                    .map_err(|e| {
                        IronCrewError::Validation(format!(
                            "PostgreSQL save_dialog_state insert error: {e}"
                        ))
                    })?
            }
            Some(current) if current == expected_revision => {
                let update_sql = format!(
                    "UPDATE {} SET flow_name = $3, agent_names = $4::jsonb, \
                     starter = $5, transcript = $6::jsonb, next_index = $7, \
                     stopped = $8, stop_reason = $9, created_at = $10, \
                     updated_at = $11, revision = revision + 1 \
                     WHERE id = $1 AND flow_path IS NOT DISTINCT FROM $2 AND revision = $12 \
                     RETURNING revision",
                    self.dialogs_table
                );
                sqlx::query_scalar(sqlx::AssertSqlSafe(update_sql))
                    .bind(&record.id)
                    .bind(&record.flow_path)
                    .bind(&record.flow_name)
                    .bind(&agents_json)
                    .bind(&record.starter)
                    .bind(&transcript_json)
                    .bind(record.next_index as i32)
                    .bind(record.stopped)
                    .bind(&record.stop_reason)
                    .bind(&record.created_at)
                    .bind(&record.updated_at)
                    .bind(expected_revision)
                    .fetch_optional(&mut *tx)
                    .await
                    .map_err(|e| {
                        IronCrewError::Validation(format!(
                            "PostgreSQL save_dialog_state update error: {e}"
                        ))
                    })?
            }
            _ => None,
        };
        let revision = revision.ok_or_else(|| {
            IronCrewError::Conflict(format!(
                "Dialog '{}' changed since revision {}; reopen it before saving",
                record.id, record.revision
            ))
        })?;
        tx.commit().await.map_err(|e| {
            IronCrewError::Validation(format!("PostgreSQL save_dialog_state commit error: {e}"))
        })?;
        u64::try_from(revision)
            .map_err(|_| IronCrewError::Validation("Invalid dialog revision".into()))
    }

    async fn get_dialog_state(
        &self,
        flow_path: Option<&str>,
        id: &str,
    ) -> Result<Option<DialogStateRecord>> {
        let sql = format!(
            "SELECT id, flow_name, flow_path, agent_names::text, starter, transcript::text, \
             next_index, stopped, stop_reason, created_at, updated_at, revision \
             FROM {} WHERE id = $1 AND ($2::TEXT IS NULL OR flow_path = $2)",
            self.dialogs_table
        );
        let row_opt = sqlx::query(sqlx::AssertSqlSafe(sql.to_string()))
            .bind(id)
            .bind(flow_path)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| {
                IronCrewError::Validation(format!("PostgreSQL get_dialog_state error: {}", e))
            })?;
        let Some(row) = row_opt else {
            return Ok(None);
        };
        let agents_str: String = row
            .try_get("agent_names")
            .map_err(|e| IronCrewError::Validation(e.to_string()))?;
        let transcript_str: String = row
            .try_get("transcript")
            .map_err(|e| IronCrewError::Validation(e.to_string()))?;
        let next_index_i32: i32 = row
            .try_get("next_index")
            .map_err(|e| IronCrewError::Validation(e.to_string()))?;
        Ok(Some(DialogStateRecord {
            id: row
                .try_get("id")
                .map_err(|e| IronCrewError::Validation(e.to_string()))?,
            flow_name: row
                .try_get("flow_name")
                .map_err(|e| IronCrewError::Validation(e.to_string()))?,
            flow_path: row
                .try_get("flow_path")
                .map_err(|e| IronCrewError::Validation(e.to_string()))?,
            agent_names: decode_stored_json(&agents_str, "dialogs.agent_names")?,
            starter: row
                .try_get("starter")
                .map_err(|e| IronCrewError::Validation(e.to_string()))?,
            transcript: decode_stored_json(&transcript_str, "dialogs.transcript")?,
            next_index: next_index_i32.max(0) as usize,
            stopped: row
                .try_get("stopped")
                .map_err(|e| IronCrewError::Validation(e.to_string()))?,
            stop_reason: row
                .try_get("stop_reason")
                .map_err(|e| IronCrewError::Validation(e.to_string()))?,
            created_at: row
                .try_get("created_at")
                .map_err(|e| IronCrewError::Validation(e.to_string()))?,
            updated_at: row
                .try_get("updated_at")
                .map_err(|e| IronCrewError::Validation(e.to_string()))?,
            revision: u64::try_from(
                row.try_get::<i64, _>("revision")
                    .map_err(|e| IronCrewError::Validation(e.to_string()))?,
            )
            .map_err(|_| IronCrewError::Validation("Invalid dialog revision".into()))?,
        }))
    }

    async fn delete_dialog_state(&self, flow_path: Option<&str>, id: &str) -> Result<()> {
        let sql = format!(
            "DELETE FROM {} WHERE id = $1 AND ($2::TEXT IS NULL OR flow_path = $2)",
            self.dialogs_table
        );
        sqlx::query(sqlx::AssertSqlSafe(sql.to_string()))
            .bind(id)
            .bind(flow_path)
            .execute(&self.pool)
            .await
            .map_err(|e| {
                IronCrewError::Validation(format!("PostgreSQL delete_dialog_state error: {}", e))
            })?;
        Ok(())
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

fn row_to_record(row: &sqlx::postgres::PgRow) -> Result<RunRecord> {
    let status_str: String = row
        .try_get("status")
        .map_err(|e| IronCrewError::Validation(format!("Column error: {}", e)))?;
    let task_results_str: String = row
        .try_get("task_results")
        .map_err(|e| IronCrewError::Validation(format!("Column error: {}", e)))?;
    let tags_str: String = row
        .try_get("tags")
        .map_err(|e| IronCrewError::Validation(format!("Column error: {}", e)))?;
    let duration_ms: i64 = row
        .try_get("duration_ms")
        .map_err(|e| IronCrewError::Validation(format!("Column error: {}", e)))?;
    let agent_count: i32 = row
        .try_get("agent_count")
        .map_err(|e| IronCrewError::Validation(format!("Column error: {}", e)))?;
    let task_count: i32 = row
        .try_get("task_count")
        .map_err(|e| IronCrewError::Validation(format!("Column error: {}", e)))?;
    let total_tokens: i32 = row
        .try_get("total_tokens")
        .map_err(|e| IronCrewError::Validation(format!("Column error: {}", e)))?;
    let cached_tokens: i32 = row
        .try_get("cached_tokens")
        .map_err(|e| IronCrewError::Validation(format!("Column error: {}", e)))?;

    Ok(RunRecord {
        run_id: row
            .try_get("run_id")
            .map_err(|e| IronCrewError::Validation(format!("Column error: {}", e)))?,
        flow_name: row
            .try_get("flow_name")
            .map_err(|e| IronCrewError::Validation(format!("Column error: {}", e)))?,
        flow: row
            .try_get("flow")
            .map_err(|e| IronCrewError::Validation(format!("Column error: {}", e)))?,
        status: status_str.parse::<RunStatus>()?,
        started_at: row
            .try_get("started_at")
            .map_err(|e| IronCrewError::Validation(format!("Column error: {}", e)))?,
        finished_at: row
            .try_get("finished_at")
            .map_err(|e| IronCrewError::Validation(format!("Column error: {}", e)))?,
        duration_ms: duration_ms as u64,
        task_results: decode_stored_json(&task_results_str, "runs.task_results")?,
        agent_count: agent_count as usize,
        task_count: task_count as usize,
        total_tokens: total_tokens as u32,
        cached_tokens: cached_tokens as u32,
        tags: decode_stored_json(&tags_str, "runs.tags")?,
        owner_instance_id: row
            .try_get("owner_instance_id")
            .map_err(|e| IronCrewError::Validation(format!("Column error: {}", e)))?,
        lease_expires_at: row
            .try_get("lease_expires_at")
            .map_err(|e| IronCrewError::Validation(format!("Column error: {}", e)))?,
    })
}

/// Convert a row from the summary query into a RunSummary (no task_results).
fn row_to_summary(row: &sqlx::postgres::PgRow) -> Result<RunSummary> {
    let status_str: String = row
        .try_get("status")
        .map_err(|e| IronCrewError::Validation(format!("Column error: {}", e)))?;
    let tags_str: String = row
        .try_get("tags")
        .map_err(|e| IronCrewError::Validation(format!("Column error: {}", e)))?;
    let duration_ms: i64 = row
        .try_get("duration_ms")
        .map_err(|e| IronCrewError::Validation(format!("Column error: {}", e)))?;
    let agent_count: i32 = row
        .try_get("agent_count")
        .map_err(|e| IronCrewError::Validation(format!("Column error: {}", e)))?;
    let task_count: i32 = row
        .try_get("task_count")
        .map_err(|e| IronCrewError::Validation(format!("Column error: {}", e)))?;
    let total_tokens: i32 = row
        .try_get("total_tokens")
        .map_err(|e| IronCrewError::Validation(format!("Column error: {}", e)))?;
    let cached_tokens: i32 = row
        .try_get("cached_tokens")
        .map_err(|e| IronCrewError::Validation(format!("Column error: {}", e)))?;

    Ok(RunSummary {
        run_id: row
            .try_get("run_id")
            .map_err(|e| IronCrewError::Validation(format!("Column error: {}", e)))?,
        flow_name: row
            .try_get("flow_name")
            .map_err(|e| IronCrewError::Validation(format!("Column error: {}", e)))?,
        flow: row
            .try_get("flow")
            .map_err(|e| IronCrewError::Validation(format!("Column error: {}", e)))?,
        status: status_str.parse::<RunStatus>()?,
        started_at: row
            .try_get("started_at")
            .map_err(|e| IronCrewError::Validation(format!("Column error: {}", e)))?,
        finished_at: row
            .try_get("finished_at")
            .map_err(|e| IronCrewError::Validation(format!("Column error: {}", e)))?,
        duration_ms: duration_ms as u64,
        agent_count: agent_count as usize,
        task_count: task_count as usize,
        total_tokens: total_tokens as u32,
        cached_tokens: cached_tokens as u32,
        tags: decode_stored_json(&tags_str, "runs.tags")?,
    })
}

fn nonnegative_accounting_value(label: &str, value: i64) -> Result<usize> {
    usize::try_from(value).map_err(|_| {
        IronCrewError::Validation(format!(
            "PostgreSQL idempotency accounting value '{label}' is negative or out of range"
        ))
    })
}

fn decode_idempotency_accounting(values: (i64, i64, i64)) -> Result<IdempotencyAccounting> {
    Ok(IdempotencyAccounting {
        records: nonnegative_accounting_value("record_count", values.0)?,
        in_flight: nonnegative_accounting_value("in_flight_count", values.1)?,
        response_bytes: nonnegative_accounting_value("response_bytes", values.2)?,
    })
}

fn accounting_row_value(row: &sqlx::postgres::PgRow, column: &str) -> Result<usize> {
    let value = row
        .try_get::<i64, _>(column)
        .map_err(|error| IronCrewError::Validation(format!("Column error: {error}")))?;
    nonnegative_accounting_value(column, value)
}

fn row_to_idempotency_record(row: &sqlx::postgres::PgRow) -> Result<IdempotencyRecord> {
    let base_revision = row
        .try_get::<Option<i64>, _>("base_revision")
        .map_err(|e| IronCrewError::Validation(format!("Column error: {e}")))?
        .map(u64::try_from)
        .transpose()
        .map_err(|_| {
            IronCrewError::Validation("PostgreSQL idempotency base_revision is negative".into())
        })?;
    let response_status = row
        .try_get::<Option<i32>, _>("response_status")
        .map_err(|e| IronCrewError::Validation(format!("Column error: {e}")))?
        .map(u16::try_from)
        .transpose()
        .map_err(|_| {
            IronCrewError::Validation(
                "PostgreSQL idempotency response_status is out of range".into(),
            )
        })?;
    let ttl_seconds = u64::try_from(
        row.try_get::<i64, _>("ttl_seconds")
            .map_err(|e| IronCrewError::Validation(format!("Column error: {e}")))?,
    )
    .map_err(|_| {
        IronCrewError::Validation("PostgreSQL idempotency ttl_seconds is negative".into())
    })?;
    let state = row
        .try_get::<String, _>("state")
        .map_err(|e| IronCrewError::Validation(format!("Column error: {e}")))?
        .parse::<IdempotencyState>()?;
    let lease_expires_at: String = row
        .try_get("lease_expires_at")
        .map_err(|e| IronCrewError::Validation(format!("Column error: {e}")))?;
    let lease_expires_at = if lease_expires_at.is_empty() {
        lease_expires_at
    } else {
        canonical_timestamp("stored idempotency lease expiry", &lease_expires_at)?
    };
    let created_at = canonical_timestamp(
        "stored idempotency creation time",
        &row.try_get::<String, _>("created_at")
            .map_err(|e| IronCrewError::Validation(format!("Column error: {e}")))?,
    )?;
    let updated_at = canonical_timestamp(
        "stored idempotency update time",
        &row.try_get::<String, _>("updated_at")
            .map_err(|e| IronCrewError::Validation(format!("Column error: {e}")))?,
    )?;
    let completed_at = row
        .try_get::<Option<String>, _>("completed_at")
        .map_err(|e| IronCrewError::Validation(format!("Column error: {e}")))?
        .map(|value| canonical_timestamp("stored idempotency completion time", &value))
        .transpose()?;
    let expires_at = row
        .try_get::<Option<String>, _>("expires_at")
        .map_err(|e| IronCrewError::Validation(format!("Column error: {e}")))?
        .map(|value| canonical_timestamp("stored idempotency retention expiry", &value))
        .transpose()?;

    let record = IdempotencyRecord {
        key_hash: row
            .try_get("key_hash")
            .map_err(|e| IronCrewError::Validation(format!("Column error: {e}")))?,
        principal_id: PrincipalId::from_digest(
            row.try_get("principal_id")
                .map_err(|e| IronCrewError::Validation(format!("Column error: {e}")))?,
        )?,
        request_fingerprint: row
            .try_get("request_fingerprint")
            .map_err(|e| IronCrewError::Validation(format!("Column error: {e}")))?,
        operation: row
            .try_get("operation")
            .map_err(|e| IronCrewError::Validation(format!("Column error: {e}")))?,
        scope: row
            .try_get("scope")
            .map_err(|e| IronCrewError::Validation(format!("Column error: {e}")))?,
        resource_id: row
            .try_get("resource_id")
            .map_err(|e| IronCrewError::Validation(format!("Column error: {e}")))?,
        exclusive_scope: row
            .try_get("exclusive_scope")
            .map_err(|e| IronCrewError::Validation(format!("Column error: {e}")))?,
        attempt_id: row
            .try_get("attempt_id")
            .map_err(|e| IronCrewError::Validation(format!("Column error: {e}")))?,
        owner_instance_id: row
            .try_get("owner_instance_id")
            .map_err(|e| IronCrewError::Validation(format!("Column error: {e}")))?,
        base_revision,
        state,
        response_status,
        response_body: row
            .try_get("response_body")
            .map_err(|e| IronCrewError::Validation(format!("Column error: {e}")))?,
        lease_expires_at,
        created_at,
        updated_at,
        completed_at,
        expires_at,
        ttl_seconds,
    };
    record.validate()?;
    Ok(record)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_grows_exponentially_then_caps() {
        let base = 1_000;
        let cap = CONNECT_BACKOFF_CAP_MS;
        assert_eq!(retry_backoff(1, base, cap), Duration::from_millis(1_000));
        assert_eq!(retry_backoff(2, base, cap), Duration::from_millis(2_000));
        assert_eq!(retry_backoff(3, base, cap), Duration::from_millis(4_000));
        assert_eq!(retry_backoff(4, base, cap), Duration::from_millis(8_000));
        assert_eq!(retry_backoff(5, base, cap), Duration::from_millis(16_000));
        // 1_000 * 2^5 = 32_000 -> capped at 30_000
        assert_eq!(retry_backoff(6, base, cap), Duration::from_millis(cap));
        assert_eq!(retry_backoff(10, base, cap), Duration::from_millis(cap));
    }

    #[test]
    fn backoff_does_not_overflow_on_large_attempts() {
        // Shift would exceed u64 width; must saturate to the cap, not panic.
        assert_eq!(
            retry_backoff(1_000, 1_000, CONNECT_BACKOFF_CAP_MS),
            Duration::from_millis(CONNECT_BACKOFF_CAP_MS)
        );
    }

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
