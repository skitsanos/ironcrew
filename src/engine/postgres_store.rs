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

        let store = Self {
            pool,
            table_name: table_name.clone(),
            conversations_table,
            dialogs_table,
            audit_events_table,
            lease,
        };
        store.bootstrap().await?;

        tracing::info!("PostgreSQL store ready (table: {})", table_name);
        Ok(store)
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
            "PostgreSQL bootstrap complete for tables '{}', '{}', '{}', '{}'",
            self.table_name,
            self.conversations_table,
            self.dialogs_table,
            self.audit_events_table
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
        let lease_expires_at = self.lease.deadline_now();
        let sql = format!(
            "INSERT INTO {} (run_id, flow_name, flow, status, started_at, finished_at, duration_ms, task_results, agent_count, task_count, total_tokens, cached_tokens, tags, owner_instance_id, lease_expires_at)
             VALUES ($1, $2, $3, 'running', $4, '', 0, $5::jsonb, $6, $7, 0, 0, $8::jsonb, $9, $10)",
            self.table_name
        );
        sqlx::query(sqlx::AssertSqlSafe(sql.to_string()))
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
            .execute(&self.pool)
            .await
            .map_err(|e| IronCrewError::Validation(format!("PG insert intent: {}", e)))?;
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
            .execute(&self.pool)
            .await
            .map_err(|e| IronCrewError::Validation(format!("PG update completion: {}", e)))?;

        if result.rows_affected() == 0 {
            let sql = format!(
                "SELECT status, owner_instance_id FROM {} WHERE run_id = $1",
                self.table_name
            );
            let row = sqlx::query(sqlx::AssertSqlSafe(sql.to_string()))
                .bind(run_id)
                .fetch_optional(&self.pool)
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
                return Ok(RunTransition::AlreadyTerminal(parsed));
            }
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
        tracing::info!("Run completion saved: {} ({})", run_id, completion.status);
        Ok(RunTransition::Applied)
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
        let deadline = self.lease.deadline_now();
        let sql = format!(
            "UPDATE {} SET lease_expires_at = $1
             WHERE owner_instance_id = $2
               AND status IN ('running', 'waiting_for_input')",
            self.table_name
        );
        let result = sqlx::query(sqlx::AssertSqlSafe(sql.to_string()))
            .bind(&deadline)
            .bind(self.lease.instance_id())
            .execute(&self.pool)
            .await
            .map_err(|e| IronCrewError::Validation(format!("PG heartbeat: {}", e)))?;
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
        transaction
            .rollback()
            .await
            .map_err(|e| IronCrewError::Validation(format!("PostgreSQL health rollback: {e}")))?;
        Ok(())
    }

    async fn reconcile_abandoned_runs(&self, now: &str) -> Result<usize> {
        let normalized_now = chrono::DateTime::parse_from_rfc3339(now)
            .map_err(|e| {
                IronCrewError::Validation(format!("Invalid reconciliation timestamp: {}", e))
            })?
            .with_timezone(&chrono::Utc)
            .to_rfc3339();
        let sql = format!(
            "UPDATE {}
             SET status = 'abandoned', finished_at = $1, lease_expires_at = ''
             WHERE status IN ('running', 'waiting_for_input')
               AND (lease_expires_at = '' OR lease_expires_at <= $2)",
            self.table_name
        );
        let result = sqlx::query(sqlx::AssertSqlSafe(sql.to_string()))
            .bind(now)
            .bind(&normalized_now)
            .execute(&self.pool)
            .await
            .map_err(|e| IronCrewError::Validation(format!("PG reconcile: {}", e)))?;
        Ok(result.rows_affected() as usize)
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
        let sql = format!(
            "DELETE FROM {} WHERE id = $1 AND ($2::TEXT IS NULL OR flow_path = $2)",
            self.conversations_table
        );
        sqlx::query(sqlx::AssertSqlSafe(sql.to_string()))
            .bind(id)
            .bind(flow_path)
            .execute(&self.pool)
            .await
            .map_err(|e| {
                IronCrewError::Validation(format!("PostgreSQL delete_conversation error: {}", e))
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
