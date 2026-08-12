use async_trait::async_trait;
use rusqlite::Connection;
use serde::de::DeserializeOwned;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use super::conversation_json::{
    preflight_conversation_execution_json, preflight_conversation_messages_json,
};
use super::conversation_record::{
    HARD_STORED_CONVERSATION_EXECUTION_BYTES, HARD_STORED_CONVERSATION_MESSAGES,
    HARD_STORED_CONVERSATION_MESSAGES_BYTES, HARD_STORED_CONVERSATION_METADATA_BYTES,
    serialize_conversation_execution, serialize_conversation_messages,
    validate_conversation_record_after_decode, validate_conversation_record_for_write,
    validate_stored_conversation_envelope, validate_stored_conversation_messages_envelope,
    validate_stored_conversation_metadata_bytes,
};
use super::idempotency::{
    CONVERSATION_MESSAGE_OPERATION, ConversationIdempotencyCommit, IdempotencyClaim,
    IdempotencyClaimOutcome, IdempotencyCompletion, IdempotencyCompletionOutcome,
    IdempotencyLimits, IdempotencyLookup, IdempotencyQuotaResource, IdempotencyQuotaScope,
    IdempotencyRecord, IdempotencyState, IdempotencyUsage, PrincipalId, RUN_OPERATION,
    RunFenceHeartbeat, validate_digest,
};
use super::run_history::{
    ListRunsFilter, RunCompletion, RunIntent, RunRecord, RunStatus, RunSummary, RunTransition,
    validate_run_id,
};
use super::sessions::{
    ConversationRecord, ConversationSummary, DialogStateRecord, validate_session_id,
};
use super::store::{RunLeaseConfig, StateStore};
use super::store_sql::{self, Dialect, SqlParam};
use crate::utils::error::{IronCrewError, Result};

pub struct SqliteStore {
    conn: Arc<Mutex<Connection>>,
    lease: RunLeaseConfig,
}

struct BoundedConversationRow {
    id: String,
    flow_name: Option<String>,
    flow_name_bytes: i64,
    flow_path: Option<String>,
    flow_path_bytes: Option<i64>,
    agent_name: Option<String>,
    agent_name_bytes: i64,
    execution: Option<String>,
    execution_bytes: i64,
    messages: Option<String>,
    messages_bytes: i64,
    message_count: Option<i64>,
    created_at: Option<String>,
    created_at_bytes: i64,
    updated_at: Option<String>,
    updated_at_bytes: i64,
    revision: i64,
}

struct BoundedConversationSummaryRow {
    id: Option<String>,
    id_bytes: i64,
    flow_path: Option<String>,
    flow_path_bytes: Option<i64>,
    agent_name: Option<String>,
    agent_name_bytes: i64,
    turn_count: i64,
    messages_bytes: i64,
    message_count: Option<i64>,
    created_at: Option<String>,
    created_at_bytes: i64,
    updated_at: Option<String>,
    updated_at_bytes: i64,
}

fn sqlite_stored_bytes(value: i64, label: &str) -> Result<u64> {
    u64::try_from(value).map_err(|_| {
        IronCrewError::Validation(format!(
            "SQLite stored conversation {label} has an invalid byte count"
        ))
    })
}

fn sqlite_bounded_metadata(value: Option<String>, bytes: i64, label: &str) -> Result<String> {
    let bytes = sqlite_stored_bytes(bytes, label)?;
    validate_stored_conversation_metadata_bytes(label, bytes)?;
    value.ok_or_else(|| {
        IronCrewError::Validation(format!(
            "SQLite stored conversation {label} could not be materialized safely"
        ))
    })
}

fn sqlite_bounded_optional_metadata(
    value: Option<String>,
    bytes: Option<i64>,
    label: &str,
) -> Result<Option<String>> {
    let Some(bytes) = bytes else {
        return Ok(None);
    };
    sqlite_bounded_metadata(value, bytes, label).map(Some)
}

fn sqlite_bounded_conversation_execution(
    value: Option<String>,
    bytes: i64,
    column: usize,
) -> Result<super::sessions::ConversationExecution> {
    let bytes = sqlite_stored_bytes(bytes, "execution")?;
    super::conversation_record::validate_stored_conversation_execution_bytes(bytes)?;
    let value = value.ok_or_else(|| {
        IronCrewError::Validation(
            "SQLite stored conversation execution identity could not be materialized safely".into(),
        )
    })?;
    preflight_conversation_execution_json(&value)?;
    decode_stored_json(&value, column).map_err(|error| {
        IronCrewError::Validation(format!(
            "SQLite stored conversation execution identity has an invalid shape: {error}"
        ))
    })
}

fn sqlite_conversation_summary(row: BoundedConversationSummaryRow) -> Result<ConversationSummary> {
    let messages_bytes = sqlite_stored_bytes(row.messages_bytes, "messages")?;
    let message_count = row
        .message_count
        .map(|count| sqlite_stored_bytes(count, "message count"))
        .transpose()?;
    validate_stored_conversation_messages_envelope(messages_bytes, message_count)?;
    let id = sqlite_bounded_metadata(row.id, row.id_bytes, "id")?;
    validate_session_id(&id)?;
    Ok(ConversationSummary {
        id,
        flow_path: sqlite_bounded_optional_metadata(
            row.flow_path,
            row.flow_path_bytes,
            "flow path",
        )?,
        agent_name: sqlite_bounded_metadata(row.agent_name, row.agent_name_bytes, "agent name")?,
        created_at: sqlite_bounded_metadata(
            row.created_at,
            row.created_at_bytes,
            "created timestamp",
        )?,
        updated_at: sqlite_bounded_metadata(
            row.updated_at,
            row.updated_at_bytes,
            "updated timestamp",
        )?,
        turn_count: usize::try_from(row.turn_count).map_err(|_| {
            IronCrewError::Validation("SQLite conversation turn count is out of range".into())
        })?,
    })
}

/// Map the shared `SqlParam` values to boxed `rusqlite::ToSql` trait objects so
/// they can be passed to `params_from_iter`. Kept as a free fn so both
/// `list`/`count` paths bind identically.
fn to_sql_params(params: Vec<SqlParam>) -> Vec<Box<dyn rusqlite::types::ToSql>> {
    params
        .into_iter()
        .map(|p| -> Box<dyn rusqlite::types::ToSql> {
            match p {
                SqlParam::Text(s) => Box::new(s),
                SqlParam::Bool(b) => Box::new(b),
            }
        })
        .collect()
}

fn decode_stored_json<T: DeserializeOwned>(raw: &str, column: usize) -> rusqlite::Result<T> {
    serde_json::from_str(raw).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            column,
            rusqlite::types::Type::Text,
            Box::new(error),
        )
    })
}

impl SqliteStore {
    pub fn new(db_path: PathBuf) -> Result<Self> {
        Self::new_with_lease_config(db_path, RunLeaseConfig::from_env()?)
    }

    pub fn new_with_lease_config(db_path: PathBuf, lease: RunLeaseConfig) -> Result<Self> {
        let conn = Connection::open(&db_path).map_err(|e| {
            IronCrewError::Validation(format!("Failed to open SQLite database: {}", e))
        })?;

        // Create tables if not exists. Three tables share the same SQLite file:
        //   runs          — historical task outputs (see run_history.rs)
        //   conversations — resumable single-agent chats (sessions.rs)
        //   dialogs       — resumable multi-agent dialogs (sessions.rs)
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS runs (
                run_id TEXT PRIMARY KEY,
                flow_name TEXT NOT NULL,
                flow TEXT NOT NULL DEFAULT '',
                status TEXT NOT NULL,
                started_at TEXT NOT NULL,
                finished_at TEXT NOT NULL,
                duration_ms INTEGER NOT NULL,
                task_results TEXT NOT NULL,
                agent_count INTEGER NOT NULL,
                task_count INTEGER NOT NULL,
                total_tokens INTEGER DEFAULT 0,
                cached_tokens INTEGER DEFAULT 0,
                tags TEXT DEFAULT '[]',
                owner_instance_id TEXT NOT NULL DEFAULT '',
                lease_expires_at TEXT NOT NULL DEFAULT '',
                created_at TEXT DEFAULT (datetime('now'))
            );

            CREATE TABLE IF NOT EXISTS conversations (
                id TEXT PRIMARY KEY,
                flow_name TEXT NOT NULL,
                agent_name TEXT NOT NULL,
                execution TEXT NOT NULL DEFAULT '{}',
                messages TEXT NOT NULL,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                revision INTEGER NOT NULL DEFAULT 0
            );

            CREATE TABLE IF NOT EXISTS dialogs (
                id TEXT PRIMARY KEY,
                flow_name TEXT NOT NULL,
                agent_names TEXT NOT NULL,
                starter TEXT NOT NULL,
                transcript TEXT NOT NULL,
                next_index INTEGER NOT NULL,
                stopped INTEGER NOT NULL DEFAULT 0,
                stop_reason TEXT,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                revision INTEGER NOT NULL DEFAULT 0
            );

            CREATE TABLE IF NOT EXISTS audit_events (
                id          TEXT PRIMARY KEY,
                timestamp   TEXT NOT NULL,
                action      TEXT NOT NULL,
                flow_path   TEXT,
                target      TEXT,
                actor       TEXT,
                source_ip   TEXT,
                success     INTEGER NOT NULL,
                status_code INTEGER NOT NULL,
                metadata    TEXT
            );
            CREATE INDEX IF NOT EXISTS idx_audit_events_timestamp_desc
                ON audit_events (timestamp DESC);
            CREATE INDEX IF NOT EXISTS idx_audit_events_flow_path
                ON audit_events (flow_path);

            CREATE TABLE IF NOT EXISTS idempotency (
                key_hash            TEXT PRIMARY KEY,
                principal_id        TEXT NOT NULL,
                request_fingerprint TEXT NOT NULL,
                operation           TEXT NOT NULL,
                scope               TEXT NOT NULL,
                resource_id         TEXT NOT NULL,
                exclusive_scope     TEXT,
                attempt_id          TEXT NOT NULL,
                owner_instance_id   TEXT NOT NULL,
                base_revision       INTEGER,
                state               TEXT NOT NULL,
                response_status     INTEGER,
                response_body       TEXT,
                lease_expires_at    TEXT NOT NULL,
                created_at          TEXT NOT NULL,
                updated_at          TEXT NOT NULL,
                completed_at        TEXT,
                expires_at          TEXT,
                ttl_seconds         INTEGER NOT NULL,
                CHECK (state IN ('claimed', 'running', 'completed', 'indeterminate')),
                CHECK (response_status IS NULL OR response_status BETWEEN 100 AND 599),
                CHECK (ttl_seconds > 0)
            );
            CREATE INDEX IF NOT EXISTS idx_idempotency_expires_at
                ON idempotency (expires_at);
            CREATE INDEX IF NOT EXISTS idx_idempotency_resource
                ON idempotency (operation, scope, resource_id);
            CREATE INDEX IF NOT EXISTS idx_idempotency_exclusive_scope
                ON idempotency (exclusive_scope);
            CREATE UNIQUE INDEX IF NOT EXISTS uniq_idempotency_active_exclusive_scope
                ON idempotency (exclusive_scope)
                WHERE exclusive_scope IS NOT NULL
                  AND state IN ('claimed', 'running');",
        )
        .map_err(|e| IronCrewError::Validation(format!("Failed to create SQLite tables: {}", e)))?;

        // Idempotent ALTER TABLE migrations for schemas predating the
        // `flow_path` column. SQLite's ADD COLUMN is atomic and safe to
        // retry; only the expected "duplicate column name" error is ignored.
        for sql in [
            "ALTER TABLE conversations ADD COLUMN flow_path TEXT",
            "ALTER TABLE conversations ADD COLUMN execution TEXT NOT NULL DEFAULT '{}'",
            "ALTER TABLE dialogs ADD COLUMN flow_path TEXT",
            "ALTER TABLE conversations ADD COLUMN revision INTEGER NOT NULL DEFAULT 0",
            "ALTER TABLE dialogs ADD COLUMN revision INTEGER NOT NULL DEFAULT 0",
        ] {
            if let Err(error) = conn.execute(sql, []) {
                let msg = error.to_string();
                if !msg.contains("duplicate column") {
                    return Err(IronCrewError::Validation(format!(
                        "SQLite session column migration failed: {error}"
                    )));
                }
            }
        }

        let principal_migration = format!(
            "ALTER TABLE idempotency ADD COLUMN principal_id TEXT NOT NULL DEFAULT '{}'",
            crate::engine::idempotency::PrincipalId::legacy().as_str()
        );
        if let Err(error) = conn.execute(&principal_migration, [])
            && !error.to_string().contains("duplicate column")
        {
            return Err(IronCrewError::Validation(format!(
                "SQLite idempotency principal migration failed: {error}"
            )));
        }
        conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_idempotency_principal ON idempotency (principal_id)",
            [],
        )
        .map_err(|error| {
            IronCrewError::Validation(format!(
                "SQLite idempotency principal index failed: {error}"
            ))
        })?;
        // Add the `flow` column to existing `runs` tables (flow-scoping the run
        // history). Detected via PRAGMA so we only ALTER when absent — mirrors
        // `migrate_sessions_to_composite_unique`'s "check first" style.
        migrate_runs_add_flow(&conn)?;
        migrate_runs_add_lease_columns(&conn)?;

        // Enforce the documented `(flow_path, id)` uniqueness on sessions.
        // Older schemas had `id TEXT PRIMARY KEY`, which meant a save from
        // flow-B would overwrite flow-A's session if they shared an id.
        // SQLite treats `NULL` values as distinct in UNIQUE indexes, so the
        // composite `UNIQUE (flow_path, id)` protects per-flow rows while
        // the save path manually de-duplicates legacy/global rows whose
        // `flow_path IS NULL`.
        //
        // Rebuilding the table is the only reliable way to drop an inline
        // PK in SQLite. We detect whether the migration is needed by
        // checking for the new composite unique index and, if absent,
        // rebuild and copy data.
        migrate_sessions_to_composite_unique(&conn)?;

        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            lease,
        })
    }
}

/// Add the `flow` column to an existing `runs` table if it isn't already
/// present. Checked via `PRAGMA table_info` so the ALTER runs at most once.
fn migrate_runs_add_flow(conn: &rusqlite::Connection) -> Result<()> {
    let mut stmt = conn
        .prepare("PRAGMA table_info(runs)")
        .map_err(|e| IronCrewError::Validation(format!("SQLite pragma prepare: {}", e)))?;
    let has_flow = stmt
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|e| IronCrewError::Validation(format!("SQLite pragma query: {}", e)))?
        .filter_map(|c| c.ok())
        .any(|name| name == "flow");
    drop(stmt);

    if !has_flow {
        conn.execute(
            "ALTER TABLE runs ADD COLUMN flow TEXT NOT NULL DEFAULT ''",
            [],
        )
        .map_err(|e| IronCrewError::Validation(format!("SQLite runs add flow column: {}", e)))?;
        tracing::info!("SQLite runs table migrated: added `flow` column");
    }
    Ok(())
}

/// Add run ownership columns to databases created before lease-based
/// reconciliation. Empty values intentionally identify legacy in-flight rows;
/// they are reconciled once instead of being attributed to the new process.
fn migrate_runs_add_lease_columns(conn: &rusqlite::Connection) -> Result<()> {
    let mut stmt = conn
        .prepare("PRAGMA table_info(runs)")
        .map_err(|e| IronCrewError::Validation(format!("SQLite pragma prepare: {}", e)))?;
    let columns: std::collections::HashSet<String> = stmt
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|e| IronCrewError::Validation(format!("SQLite pragma query: {}", e)))?
        .filter_map(|column| column.ok())
        .collect();
    drop(stmt);

    for (column, sql) in [
        (
            "owner_instance_id",
            "ALTER TABLE runs ADD COLUMN owner_instance_id TEXT NOT NULL DEFAULT ''",
        ),
        (
            "lease_expires_at",
            "ALTER TABLE runs ADD COLUMN lease_expires_at TEXT NOT NULL DEFAULT ''",
        ),
    ] {
        if !columns.contains(column) {
            conn.execute(sql, []).map_err(|e| {
                IronCrewError::Validation(format!("SQLite runs add {} column: {}", column, e))
            })?;
        }
    }
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_runs_active_lease ON runs (status, lease_expires_at)",
        [],
    )
    .map_err(|e| IronCrewError::Validation(format!("SQLite lease index: {}", e)))?;
    Ok(())
}

/// Rebuild `conversations` and `dialogs` so the effective unique key is
/// `(flow_path, id)` rather than `id` alone. Safe to run repeatedly.
fn migrate_sessions_to_composite_unique(conn: &rusqlite::Connection) -> Result<()> {
    let already_migrated: i64 = conn
        .query_row(
            "SELECT count(*) FROM sqlite_master \
             WHERE type='index' AND name='uniq_conversations_flow_id'",
            [],
            |row| row.get(0),
        )
        .unwrap_or(0);
    if already_migrated > 0 {
        return Ok(());
    }

    // Rebuild conversations.
    conn.execute_batch(
        "BEGIN;
         CREATE TABLE IF NOT EXISTS conversations_new (
             id          TEXT NOT NULL,
             flow_name   TEXT NOT NULL,
             flow_path   TEXT,
             agent_name  TEXT NOT NULL,
             execution   TEXT NOT NULL DEFAULT '{}',
             messages    TEXT NOT NULL,
             created_at  TEXT NOT NULL,
             updated_at  TEXT NOT NULL,
             revision    INTEGER NOT NULL DEFAULT 0,
             UNIQUE (flow_path, id)
         );
         INSERT OR IGNORE INTO conversations_new
             (id, flow_name, flow_path, agent_name, execution, messages, created_at, updated_at, revision)
             SELECT id, flow_name, flow_path, agent_name, execution, messages, created_at, updated_at, revision
             FROM conversations;
         DROP TABLE conversations;
         ALTER TABLE conversations_new RENAME TO conversations;
         CREATE UNIQUE INDEX IF NOT EXISTS uniq_conversations_flow_id
             ON conversations (flow_path, id);
         COMMIT;",
    )
    .map_err(|e| {
        IronCrewError::Validation(format!("SQLite conversations migration failed: {}", e))
    })?;

    // Rebuild dialogs.
    conn.execute_batch(
        "BEGIN;
         CREATE TABLE IF NOT EXISTS dialogs_new (
             id          TEXT NOT NULL,
             flow_name   TEXT NOT NULL,
             flow_path   TEXT,
             agent_names TEXT NOT NULL,
             starter     TEXT NOT NULL,
             transcript  TEXT NOT NULL,
             next_index  INTEGER NOT NULL,
             stopped     INTEGER NOT NULL DEFAULT 0,
             stop_reason TEXT,
             created_at  TEXT NOT NULL,
             updated_at  TEXT NOT NULL,
             revision    INTEGER NOT NULL DEFAULT 0,
             UNIQUE (flow_path, id)
         );
         INSERT OR IGNORE INTO dialogs_new
             (id, flow_name, flow_path, agent_names, starter, transcript,
              next_index, stopped, stop_reason, created_at, updated_at, revision)
             SELECT id, flow_name, flow_path, agent_names, starter, transcript,
                    next_index, stopped, stop_reason, created_at, updated_at, revision
             FROM dialogs;
         DROP TABLE dialogs;
         ALTER TABLE dialogs_new RENAME TO dialogs;
         CREATE UNIQUE INDEX IF NOT EXISTS uniq_dialogs_flow_id
             ON dialogs (flow_path, id);
         COMMIT;",
    )
    .map_err(|e| IronCrewError::Validation(format!("SQLite dialogs migration failed: {}", e)))?;

    tracing::info!("SQLite sessions migrated to composite (flow_path, id) uniqueness");
    Ok(())
}

/// Flatten a `spawn_blocking` join result: a panicked/cancelled blocking task
/// becomes a `Validation` error, otherwise the inner `Result<T>` is returned.
fn flatten_join<T>(joined: std::result::Result<Result<T>, tokio::task::JoinError>) -> Result<T> {
    match joined {
        Ok(inner) => inner,
        Err(e) => Err(IronCrewError::Validation(format!(
            "SQLite blocking task failed: {}",
            e
        ))),
    }
}

fn sqlite_idempotency_select_columns() -> String {
    format!(
        "key_hash, request_fingerprint, operation, scope, resource_id, exclusive_scope, \
         attempt_id, owner_instance_id, base_revision, state, response_status, \
         CASE WHEN response_body IS NULL OR length(CAST(response_body AS BLOB)) <= {limit} \
              THEN response_body END AS response_body, \
         lease_expires_at, created_at, updated_at, completed_at, expires_at, ttl_seconds, \
         principal_id, COALESCE(length(CAST(response_body AS BLOB)), 0) AS response_body_bytes",
        limit = super::idempotency::HARD_IDEMPOTENCY_RESPONSE_BYTES,
    )
}

fn sqlite_idempotency_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<IdempotencyRecord> {
    let response_body_bytes = usize::try_from(row.get::<_, i64>(19)?)
        .map_err(|_| rusqlite::Error::IntegralValueOutOfRange(19, i64::MIN))?;
    if response_body_bytes > super::idempotency::HARD_IDEMPOTENCY_RESPONSE_BYTES {
        return Err(rusqlite::Error::FromSqlConversionFailure(
            19,
            rusqlite::types::Type::Integer,
            Box::new(IronCrewError::Validation(
                "Stored idempotency response body exceeds the hard byte limit".into(),
            )),
        ));
    }
    let state_raw: String = row.get(9)?;
    let state = state_raw.parse::<IdempotencyState>().map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(9, rusqlite::types::Type::Text, Box::new(error))
    })?;
    let base_revision = row
        .get::<_, Option<i64>>(8)?
        .map(u64::try_from)
        .transpose()
        .map_err(|_| rusqlite::Error::IntegralValueOutOfRange(8, i64::MIN))?;
    let response_status = row
        .get::<_, Option<i64>>(10)?
        .map(u16::try_from)
        .transpose()
        .map_err(|_| rusqlite::Error::IntegralValueOutOfRange(10, i64::MIN))?;
    let ttl_seconds = u64::try_from(row.get::<_, i64>(17)?)
        .map_err(|_| rusqlite::Error::IntegralValueOutOfRange(17, i64::MIN))?;
    Ok(IdempotencyRecord {
        key_hash: row.get(0)?,
        principal_id: PrincipalId::from_digest(row.get(18)?).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                18,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })?,
        request_fingerprint: row.get(1)?,
        operation: row.get(2)?,
        scope: row.get(3)?,
        resource_id: row.get(4)?,
        exclusive_scope: row.get(5)?,
        attempt_id: row.get(6)?,
        owner_instance_id: row.get(7)?,
        base_revision,
        state,
        response_status,
        response_body: row.get(11)?,
        lease_expires_at: row.get(12)?,
        created_at: row.get(13)?,
        updated_at: row.get(14)?,
        completed_at: row.get(15)?,
        expires_at: row.get(16)?,
        ttl_seconds,
    })
}

fn sqlite_idempotency_record(
    conn: &rusqlite::Connection,
    key_hash: &str,
) -> Result<Option<IdempotencyRecord>> {
    let columns = sqlite_idempotency_select_columns();
    let sql = format!("SELECT {columns} FROM idempotency WHERE key_hash = ?1");
    match conn.query_row(&sql, rusqlite::params![key_hash], sqlite_idempotency_row) {
        Ok(record) => {
            record.validate()?;
            Ok(Some(record))
        }
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(error) => Err(IronCrewError::Validation(format!(
            "SQLite idempotency query error: {error}"
        ))),
    }
}

fn sqlite_active_exclusive_record(
    conn: &rusqlite::Connection,
    exclusive_scope: &str,
) -> Result<Option<IdempotencyRecord>> {
    let columns = sqlite_idempotency_select_columns();
    let sql = format!(
        "SELECT {columns} FROM idempotency \
         WHERE exclusive_scope = ?1 AND state IN ('claimed', 'running')"
    );
    match conn.query_row(
        &sql,
        rusqlite::params![exclusive_scope],
        sqlite_idempotency_row,
    ) {
        Ok(record) => {
            record.validate()?;
            Ok(Some(record))
        }
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(error) => Err(IronCrewError::Validation(format!(
            "SQLite active idempotency query error: {error}"
        ))),
    }
}

fn sqlite_indeterminate_exclusive_records(
    conn: &rusqlite::Connection,
    exclusive_scope: &str,
) -> Result<Vec<IdempotencyRecord>> {
    let columns = sqlite_idempotency_select_columns();
    let sql = format!(
        "SELECT {columns} FROM idempotency \
         WHERE exclusive_scope = ?1 AND state = 'indeterminate'"
    );
    let mut statement = conn.prepare(&sql).map_err(|error| {
        IronCrewError::Validation(format!(
            "SQLite indeterminate idempotency hazard prepare error: {error}"
        ))
    })?;
    let rows = statement
        .query_map(rusqlite::params![exclusive_scope], sqlite_idempotency_row)
        .map_err(|error| {
            IronCrewError::Validation(format!(
                "SQLite indeterminate idempotency hazard query error: {error}"
            ))
        })?;
    let mut records = Vec::new();
    for row in rows {
        let record = row.map_err(|error| {
            IronCrewError::Validation(format!(
                "SQLite indeterminate idempotency hazard row error: {error}"
            ))
        })?;
        record.validate()?;
        records.push(record);
    }
    Ok(records)
}

fn sqlite_parse_idempotency_time(
    label: &str,
    value: &str,
) -> Result<chrono::DateTime<chrono::Utc>> {
    chrono::DateTime::parse_from_rfc3339(value)
        .map(|timestamp| timestamp.with_timezone(&chrono::Utc))
        .map_err(|error| IronCrewError::Validation(format!("{label} is not RFC3339: {error}")))
}

fn sqlite_deadline_passed(deadline: &str, now: &str) -> Result<bool> {
    Ok(
        sqlite_parse_idempotency_time("idempotency deadline", deadline)?
            <= sqlite_parse_idempotency_time("idempotency current time", now)?,
    )
}

fn sqlite_quota_retry_after(
    conn: &rusqlite::Connection,
    principal_id: Option<&PrincipalId>,
    now: &str,
) -> Result<u64> {
    let deadline: Option<String> = if let Some(principal_id) = principal_id {
        conn.query_row(
            "SELECT MIN(CASE WHEN state IN ('claimed', 'running') \
                        THEN lease_expires_at ELSE expires_at END) \
             FROM idempotency WHERE principal_id = ?1",
            rusqlite::params![principal_id.as_str()],
            |row| row.get(0),
        )
    } else {
        conn.query_row(
            "SELECT MIN(CASE WHEN state IN ('claimed', 'running') \
                        THEN lease_expires_at ELSE expires_at END) FROM idempotency",
            [],
            |row| row.get(0),
        )
    }
    .map_err(|error| {
        IronCrewError::Validation(format!(
            "SQLite idempotency capacity deadline query error: {error}"
        ))
    })?;
    let Some(deadline) = deadline else {
        return Ok(60);
    };
    let deadline = sqlite_parse_idempotency_time("idempotency capacity deadline", &deadline)?;
    let now = sqlite_parse_idempotency_time("idempotency capacity clock", now)?;
    let milliseconds = deadline
        .signed_duration_since(now)
        .num_milliseconds()
        .max(1);
    Ok(u64::try_from(milliseconds.saturating_add(999) / 1_000)
        .unwrap_or(u64::MAX)
        .max(1))
}

fn sqlite_quota_at_or_above(value: usize, limit: usize, percentage: usize) -> bool {
    value >= limit.saturating_mul(percentage).saturating_add(99) / 100
}

fn sqlite_recovery_grace_elapsed(
    hazard: &IdempotencyRecord,
    claim_time: &str,
    ttl: std::time::Duration,
) -> Result<bool> {
    let marked_at = hazard
        .completed_at
        .as_deref()
        .unwrap_or(hazard.updated_at.as_str());
    let marked_at = sqlite_parse_idempotency_time("idempotency hazard time", marked_at)?;
    let claim_time = sqlite_parse_idempotency_time("idempotency recovery claim time", claim_time)?;
    let grace = chrono::Duration::from_std(ttl).map_err(|error| {
        IronCrewError::Validation(format!(
            "Idempotency recovery grace is out of range: {error}"
        ))
    })?;
    let recovery_at = marked_at.checked_add_signed(grace).ok_or_else(|| {
        IronCrewError::Validation("Idempotency recovery grace deadline overflow".into())
    })?;
    Ok(claim_time >= recovery_at)
}

fn sqlite_later_lease(existing: &str, proposed: &str) -> Result<String> {
    if existing.is_empty() {
        return Ok(proposed.to_string());
    }
    let existing_time = sqlite_parse_idempotency_time("existing run lease expiry", existing)?;
    let proposed_time = sqlite_parse_idempotency_time("proposed run lease expiry", proposed)?;
    Ok(if existing_time >= proposed_time {
        existing.to_string()
    } else {
        proposed.to_string()
    })
}

fn sqlite_retention_expiry(now: &str, ttl_seconds: u64) -> Result<String> {
    let now = sqlite_parse_idempotency_time("idempotency current time", now)?;
    let ttl = i64::try_from(ttl_seconds)
        .map_err(|_| IronCrewError::Validation("Idempotency TTL is out of range".into()))?;
    now.checked_add_signed(chrono::Duration::seconds(ttl))
        .ok_or_else(|| IronCrewError::Validation("Idempotency retention expiry overflow".into()))
        .map(|timestamp| timestamp.to_rfc3339())
}

fn sqlite_prune_idempotency(conn: &rusqlite::Connection, now: &str, limit: usize) -> Result<usize> {
    sqlite_parse_idempotency_time("idempotency prune time", now)?;
    if limit == 0 {
        return Ok(0);
    }
    let mut statement = conn
        .prepare(
            "SELECT key_hash, expires_at FROM idempotency \
             WHERE state IN ('completed', 'indeterminate') AND expires_at IS NOT NULL",
        )
        .map_err(|error| {
            IronCrewError::Validation(format!("SQLite idempotency prune prepare error: {error}"))
        })?;
    let rows = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(|error| {
            IronCrewError::Validation(format!("SQLite idempotency prune query error: {error}"))
        })?;
    let mut expired = Vec::new();
    for row in rows {
        let (key_hash, expires_at) = row.map_err(|error| {
            IronCrewError::Validation(format!("SQLite idempotency prune row error: {error}"))
        })?;
        if sqlite_deadline_passed(&expires_at, now)? {
            expired.push((expires_at, key_hash));
        }
    }
    drop(statement);
    expired.sort_by(|left, right| left.0.cmp(&right.0));
    let mut removed = 0usize;
    for (_, key_hash) in expired.into_iter().take(limit) {
        removed += conn
            .execute(
                "DELETE FROM idempotency WHERE key_hash = ?1",
                rusqlite::params![key_hash],
            )
            .map_err(|error| {
                IronCrewError::Validation(format!("SQLite idempotency prune error: {error}"))
            })?;
    }
    Ok(removed)
}

fn sqlite_response_bytes(
    conn: &rusqlite::Connection,
    principal_id: &PrincipalId,
    except_key_hash: &str,
) -> Result<(usize, usize)> {
    let total: i64 = conn
        .query_row(
            "SELECT COALESCE(SUM(length(CAST(response_body AS BLOB))), 0) \
             FROM idempotency WHERE key_hash != ?1 AND response_body IS NOT NULL",
            rusqlite::params![except_key_hash],
            |row| row.get(0),
        )
        .map_err(|error| {
            IronCrewError::Validation(format!(
                "SQLite idempotency response byte query error: {error}"
            ))
        })?;
    let principal_total: i64 = conn
        .query_row(
            "SELECT COALESCE(SUM(length(CAST(response_body AS BLOB))), 0) \
             FROM idempotency \
             WHERE key_hash != ?1 AND response_body IS NOT NULL AND principal_id = ?2",
            rusqlite::params![except_key_hash, principal_id.as_str()],
            |row| row.get(0),
        )
        .map_err(|error| {
            IronCrewError::Validation(format!(
                "SQLite principal idempotency response byte query error: {error}"
            ))
        })?;
    let total = usize::try_from(total).map_err(|_| {
        IronCrewError::Validation("Idempotency response byte total is out of range".into())
    })?;
    let principal_total = usize::try_from(principal_total).map_err(|_| {
        IronCrewError::Validation(
            "Principal idempotency response byte total is out of range".into(),
        )
    })?;
    Ok((total, principal_total))
}

fn sqlite_completion_fence(
    record: &IdempotencyRecord,
    completion: &IdempotencyCompletion,
) -> Result<()> {
    if record.principal_id != completion.principal_id
        || record.request_fingerprint != completion.request_fingerprint
        || record.attempt_id != completion.attempt_id
        || record.owner_instance_id != completion.owner_instance_id
    {
        return Err(IronCrewError::Conflict(
            "Idempotency operation changed before completion".into(),
        ));
    }
    Ok(())
}

fn sqlite_write_indeterminate(
    conn: &rusqlite::Connection,
    record: &mut IdempotencyRecord,
    completed_at: &str,
    expires_at: &str,
) -> Result<()> {
    record.state = IdempotencyState::Indeterminate;
    record.response_status = None;
    record.response_body = None;
    record.updated_at = completed_at.to_string();
    record.completed_at = Some(completed_at.to_string());
    record.expires_at = Some(expires_at.to_string());
    record.validate()?;
    let changed = conn
        .execute(
            "UPDATE idempotency SET state = 'indeterminate', response_status = NULL, \
             response_body = NULL, updated_at = ?1, completed_at = ?1, expires_at = ?2 \
             WHERE key_hash = ?3 AND attempt_id = ?4 \
               AND state IN ('claimed', 'running')",
            rusqlite::params![
                completed_at,
                expires_at,
                &record.key_hash,
                &record.attempt_id
            ],
        )
        .map_err(|error| {
            IronCrewError::Validation(format!(
                "SQLite idempotency indeterminate update error: {error}"
            ))
        })?;
    if changed != 1 {
        return Err(IronCrewError::Conflict(
            "Idempotency operation changed during indeterminate transition".into(),
        ));
    }
    Ok(())
}

fn sqlite_active_conversation_idempotency(
    conn: &rusqlite::Connection,
    flow_path: Option<&str>,
    resource_id: &str,
    any_scope_when_none: bool,
) -> Result<bool> {
    let count: i64 = if flow_path.is_none() && any_scope_when_none {
        conn.query_row(
            "SELECT COUNT(*) FROM idempotency \
             WHERE operation = ?1 AND resource_id = ?2 \
               AND state IN ('claimed', 'running')",
            rusqlite::params![CONVERSATION_MESSAGE_OPERATION, resource_id],
            |row| row.get(0),
        )
    } else {
        conn.query_row(
            "SELECT COUNT(*) FROM idempotency \
             WHERE operation = ?1 AND scope = ?2 AND resource_id = ?3 \
               AND state IN ('claimed', 'running')",
            rusqlite::params![
                CONVERSATION_MESSAGE_OPERATION,
                flow_path.unwrap_or(""),
                resource_id
            ],
            |row| row.get(0),
        )
    }
    .map_err(|error| {
        IronCrewError::Validation(format!(
            "SQLite active conversation idempotency query error: {error}"
        ))
    })?;
    Ok(count > 0)
}

fn sqlite_complete_run_idempotency(
    conn: &rusqlite::Connection,
    run_id: &str,
    completed_at: &str,
) -> Result<usize> {
    sqlite_parse_idempotency_time("run idempotency completion time", completed_at)?;
    let mut statement = conn
        .prepare(
            "SELECT key_hash, ttl_seconds FROM idempotency \
             WHERE operation = ?1 AND resource_id = ?2 \
               AND state IN ('claimed', 'running', 'indeterminate')",
        )
        .map_err(|error| {
            IronCrewError::Validation(format!(
                "SQLite run idempotency completion prepare error: {error}"
            ))
        })?;
    let rows = statement
        .query_map(rusqlite::params![RUN_OPERATION, run_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })
        .map_err(|error| {
            IronCrewError::Validation(format!(
                "SQLite run idempotency completion query error: {error}"
            ))
        })?;
    let mut records = Vec::new();
    for row in rows {
        let (key_hash, ttl) = row.map_err(|error| {
            IronCrewError::Validation(format!(
                "SQLite run idempotency completion row error: {error}"
            ))
        })?;
        records.push((
            key_hash,
            u64::try_from(ttl)
                .map_err(|_| IronCrewError::Validation("Idempotency TTL is out of range".into()))?,
        ));
    }
    drop(statement);

    let mut changed = 0usize;
    for (key_hash, ttl_seconds) in records {
        let expires_at = sqlite_retention_expiry(completed_at, ttl_seconds)?;
        changed = changed.saturating_add(
            conn.execute(
                "UPDATE idempotency SET state = 'completed', lease_expires_at = '', \
                 updated_at = ?1, completed_at = ?1, expires_at = ?2 \
                 WHERE key_hash = ?3 \
                   AND state IN ('claimed', 'running', 'indeterminate')",
                rusqlite::params![completed_at, expires_at, key_hash],
            )
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "SQLite run idempotency completion update error: {error}"
                ))
            })?,
        );
    }
    Ok(changed)
}

fn sqlite_complete_terminal_run_idempotency(conn: &rusqlite::Connection) -> Result<usize> {
    let mut statement = conn
        .prepare(
            "SELECT idem.key_hash, idem.ttl_seconds, run.finished_at \
             FROM idempotency AS idem \
             JOIN runs AS run ON run.run_id = idem.resource_id \
             WHERE idem.operation = ?1 \
               AND idem.state IN ('claimed', 'running', 'indeterminate') \
               AND run.status NOT IN ('running', 'waiting_for_input')",
        )
        .map_err(|error| {
            IronCrewError::Validation(format!(
                "SQLite reconciled run idempotency prepare error: {error}"
            ))
        })?;
    let rows = statement
        .query_map(rusqlite::params![RUN_OPERATION], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
            ))
        })
        .map_err(|error| {
            IronCrewError::Validation(format!(
                "SQLite reconciled run idempotency query error: {error}"
            ))
        })?;
    let mut records = Vec::new();
    for row in rows {
        let (key_hash, ttl, finished_at) = row.map_err(|error| {
            IronCrewError::Validation(format!(
                "SQLite reconciled run idempotency row error: {error}"
            ))
        })?;
        records.push((
            key_hash,
            u64::try_from(ttl)
                .map_err(|_| IronCrewError::Validation("Idempotency TTL is out of range".into()))?,
            finished_at,
        ));
    }
    drop(statement);

    let mut changed = 0usize;
    for (key_hash, ttl_seconds, finished_at) in records {
        let expires_at = sqlite_retention_expiry(&finished_at, ttl_seconds)?;
        changed = changed.saturating_add(
            conn.execute(
                "UPDATE idempotency SET state = 'completed', lease_expires_at = '', \
                 updated_at = ?1, completed_at = ?1, expires_at = ?2 \
                 WHERE key_hash = ?3 \
                   AND state IN ('claimed', 'running', 'indeterminate')",
                rusqlite::params![finished_at, expires_at, key_hash],
            )
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "SQLite reconciled run idempotency update error: {error}"
                ))
            })?,
        );
    }
    Ok(changed)
}

fn sqlite_reconcile_expired_conversation_idempotency(
    conn: &rusqlite::Connection,
    now: &str,
) -> Result<usize> {
    let columns = sqlite_idempotency_select_columns();
    let sql = format!(
        "SELECT {columns} FROM idempotency \
         WHERE operation = ?1 AND state IN ('claimed', 'running')"
    );
    let mut statement = conn.prepare(&sql).map_err(|error| {
        IronCrewError::Validation(format!(
            "SQLite conversation idempotency reconciliation prepare error: {error}"
        ))
    })?;
    let rows = statement
        .query_map(
            rusqlite::params![CONVERSATION_MESSAGE_OPERATION],
            sqlite_idempotency_row,
        )
        .map_err(|error| {
            IronCrewError::Validation(format!(
                "SQLite conversation idempotency reconciliation query error: {error}"
            ))
        })?;
    let mut expired = Vec::new();
    for row in rows {
        let record = row.map_err(|error| {
            IronCrewError::Validation(format!(
                "SQLite conversation idempotency reconciliation row error: {error}"
            ))
        })?;
        record.validate()?;
        if sqlite_deadline_passed(&record.lease_expires_at, now)? {
            expired.push(record);
        }
    }
    drop(statement);

    let mut changed = 0usize;
    for mut record in expired {
        let expires_at = sqlite_retention_expiry(now, record.ttl_seconds)?;
        sqlite_write_indeterminate(conn, &mut record, now, &expires_at)?;
        changed = changed.saturating_add(1);
    }
    Ok(changed)
}

#[async_trait]
impl StateStore for SqliteStore {
    async fn save_run_intent(&self, intent: RunIntent) -> Result<String> {
        let conn = Arc::clone(&self.conn);
        let owner_instance_id = self.lease.instance_id().to_string();
        let lease_expires_at = self.lease.deadline_now();
        let may_hydrate = intent.suggested_id.is_some();

        flatten_join(
            tokio::task::spawn_blocking(move || {
                let mut conn = conn
                    .lock()
                    .map_err(|e| IronCrewError::Validation(format!("SQLite lock error: {}", e)))?;
                let run_id = intent
                    .suggested_id
                    .clone()
                    .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
                let tags_json = serde_json::to_string(&intent.tags).map_err(|e| {
                    IronCrewError::Validation(format!("Failed to serialize tags: {}", e))
                })?;
                let agent_count = i64::try_from(intent.agent_count).map_err(|_| {
                    IronCrewError::Validation("Agent count is out of range".into())
                })?;
                let task_count = i64::try_from(intent.task_count).map_err(|_| {
                    IronCrewError::Validation("Task count is out of range".into())
                })?;
                let tx = conn
                    .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                    .map_err(|error| {
                        IronCrewError::Validation(format!(
                            "SQLite insert intent transaction error: {error}"
                        ))
                    })?;

                let existing = match tx.query_row(
                    "SELECT status, flow, owner_instance_id, lease_expires_at \
                     FROM runs WHERE run_id = ?1",
                    rusqlite::params![&run_id],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, String>(3)?,
                        ))
                    },
                ) {
                    Ok(existing) => Some(existing),
                    Err(rusqlite::Error::QueryReturnedNoRows) => None,
                    Err(error) => {
                        return Err(IronCrewError::Validation(format!(
                            "SQLite existing run intent query error: {error}"
                        )));
                    }
                };

                if let Some((status, flow, owner, existing_lease)) = existing {
                    let duplicate_error = || {
                        IronCrewError::Validation(format!("Run '{run_id}' already exists"))
                    };
                    if !may_hydrate
                        || !status.parse::<RunStatus>()?.is_in_flight()
                        || owner != owner_instance_id
                        || flow != intent.flow
                    {
                        return Err(duplicate_error());
                    }

                    let mut statement = tx
                        .prepare(
                            "SELECT key_hash, state, lease_expires_at FROM idempotency \
                             WHERE operation = ?1 AND scope = ?2 AND resource_id = ?3 \
                               AND owner_instance_id = ?4 AND state IN ('running', 'completed')",
                        )
                        .map_err(|error| {
                            IronCrewError::Validation(format!(
                                "SQLite run hydration ledger prepare error: {error}"
                            ))
                        })?;
                    let rows = statement
                        .query_map(
                            rusqlite::params![
                                RUN_OPERATION,
                                &intent.flow,
                                &run_id,
                                &owner_instance_id,
                            ],
                            |row| {
                                Ok((
                                    row.get::<_, String>(0)?,
                                    row.get::<_, String>(1)?,
                                    row.get::<_, String>(2)?,
                                ))
                            },
                        )
                        .map_err(|error| {
                            IronCrewError::Validation(format!(
                                "SQLite run hydration ledger query error: {error}"
                            ))
                        })?;
                    let mut ledgers = Vec::new();
                    for row in rows {
                        ledgers.push(row.map_err(|error| {
                            IronCrewError::Validation(format!(
                                "SQLite run hydration ledger row error: {error}"
                            ))
                        })?);
                    }
                    drop(statement);
                    if ledgers.len() != 1 {
                        return Err(duplicate_error());
                    }
                    let (ledger_key, ledger_state, ledger_lease) =
                        ledgers.pop().ok_or_else(duplicate_error)?;
                    let ledger_state = ledger_state.parse::<IdempotencyState>()?;
                    let mut hydrated_lease =
                        sqlite_later_lease(&existing_lease, &lease_expires_at)?;
                    if ledger_state == IdempotencyState::Running {
                        hydrated_lease = sqlite_later_lease(&hydrated_lease, &ledger_lease)?;
                        tx.execute(
                            "UPDATE idempotency SET lease_expires_at = ?1 \
                             WHERE key_hash = ?2 AND state = 'running'",
                            rusqlite::params![&hydrated_lease, &ledger_key],
                        )
                        .map_err(|error| {
                            IronCrewError::Validation(format!(
                                "SQLite run hydration ledger update error: {error}"
                            ))
                        })?;
                    }
                    let changed = tx
                        .execute(
                            "UPDATE runs SET flow_name = ?1, agent_count = ?2, task_count = ?3, \
                             tags = ?4, lease_expires_at = ?5 \
                             WHERE run_id = ?6 AND owner_instance_id = ?7 \
                               AND status IN ('running', 'waiting_for_input')",
                            rusqlite::params![
                                &intent.flow_name,
                                agent_count,
                                task_count,
                                &tags_json,
                                &hydrated_lease,
                                &run_id,
                                &owner_instance_id,
                            ],
                        )
                        .map_err(|error| {
                            IronCrewError::Validation(format!(
                                "SQLite provisional run hydration error: {error}"
                            ))
                        })?;
                    if changed != 1 {
                        return Err(IronCrewError::Conflict(format!(
                            "Run '{run_id}' changed during provisional hydration"
                        )));
                    }
                    tx.commit().map_err(|error| {
                        IronCrewError::Validation(format!(
                            "SQLite provisional run hydration commit error: {error}"
                        ))
                    })?;
                    tracing::debug!("Provisional run intent hydrated: {run_id}");
                    return Ok(run_id);
                }

                tx.execute(
                    "INSERT INTO runs (run_id, flow_name, flow, status, started_at, finished_at, duration_ms, task_results, agent_count, task_count, total_tokens, cached_tokens, tags, owner_instance_id, lease_expires_at)
                     VALUES (?1, ?2, ?3, 'running', ?4, '', 0, '[]', ?5, ?6, 0, 0, ?7, ?8, ?9)",
                    rusqlite::params![
                        &run_id,
                        &intent.flow_name,
                        &intent.flow,
                        &intent.started_at,
                        agent_count,
                        task_count,
                        &tags_json,
                        &owner_instance_id,
                        &lease_expires_at,
                    ],
                )
                .map_err(|e| IronCrewError::Validation(format!("SQLite insert intent: {}", e)))?;
                tx.execute(
                    "UPDATE idempotency SET state = 'running', lease_expires_at = ?1, \
                     updated_at = ?2 \
                     WHERE operation = ?3 AND scope = ?4 AND resource_id = ?5 \
                       AND owner_instance_id = ?6 AND state = 'claimed'",
                    rusqlite::params![
                        &lease_expires_at,
                        &intent.started_at,
                        RUN_OPERATION,
                        &intent.flow,
                        &run_id,
                        &owner_instance_id,
                    ],
                )
                .map_err(|error| {
                    IronCrewError::Validation(format!(
                        "SQLite run idempotency mapping transition error: {error}"
                    ))
                })?;
                tx.commit().map_err(|error| {
                    IronCrewError::Validation(format!(
                        "SQLite insert intent commit error: {error}"
                    ))
                })?;
                tracing::debug!("Run intent saved: {}", run_id);
                Ok(run_id)
            })
            .await,
        )
    }

    async fn update_run_completion(
        &self,
        run_id: &str,
        completion: RunCompletion,
    ) -> Result<RunTransition> {
        completion.validate()?;
        let conn = Arc::clone(&self.conn);
        let run_id = run_id.to_string();
        let owner_instance_id = self.lease.instance_id().to_string();

        flatten_join(
            tokio::task::spawn_blocking(move || {
                let mut conn = conn
                    .lock()
                    .map_err(|e| IronCrewError::Validation(format!("SQLite lock error: {}", e)))?;
                let task_results_json =
                    serde_json::to_string(&completion.task_results).map_err(|e| {
                        IronCrewError::Validation(format!(
                            "Failed to serialize task_results: {}",
                            e
                        ))
                    })?;
                let tx = conn
                    .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                    .map_err(|error| {
                        IronCrewError::Validation(format!(
                            "SQLite update completion transaction error: {error}"
                        ))
                    })?;
                let rows = tx
                    .execute(
                        "UPDATE runs
                         SET status = ?1, finished_at = ?2, duration_ms = ?3,
                             task_results = ?4, total_tokens = ?5, cached_tokens = ?6,
                             lease_expires_at = ''
                         WHERE run_id = ?7 AND status IN ('running', 'waiting_for_input')
                           AND owner_instance_id = ?8",
                        rusqlite::params![
                            completion.status.to_string(),
                            &completion.finished_at,
                            i64::try_from(completion.duration_ms).map_err(|_| {
                                IronCrewError::Validation("Run duration is out of range".into())
                            })?,
                            &task_results_json,
                            i64::from(completion.total_tokens),
                            i64::from(completion.cached_tokens),
                            &run_id,
                            &owner_instance_id,
                        ],
                    )
                    .map_err(|e| {
                        IronCrewError::Validation(format!("SQLite update completion: {}", e))
                    })?;

                let (transition, finished_at) = if rows == 0 {
                    let state = tx.query_row(
                        "SELECT status, owner_instance_id, finished_at FROM runs WHERE run_id = ?1",
                        rusqlite::params![&run_id],
                        |row| {
                            Ok((
                                row.get::<_, String>(0)?,
                                row.get::<_, String>(1)?,
                                row.get::<_, String>(2)?,
                            ))
                        },
                    );
                    match state {
                        Ok((status, _, stored_finished_at))
                            if status.parse::<RunStatus>()?.is_terminal() =>
                        {
                            (
                                RunTransition::AlreadyTerminal(status.parse()?),
                                stored_finished_at,
                            )
                        }
                        Ok((_, owner, _)) => {
                            return Err(IronCrewError::Validation(format!(
                                "Run '{}' is owned by instance '{}', not '{}'",
                                run_id, owner, owner_instance_id
                            )));
                        }
                        Err(rusqlite::Error::QueryReturnedNoRows) => {
                            return Err(IronCrewError::Validation(format!(
                                "Run '{}' not found",
                                run_id
                            )));
                        }
                        Err(error) => {
                            return Err(IronCrewError::Validation(format!(
                                "SQLite completion state query: {error}"
                            )));
                        }
                    }
                } else {
                    (RunTransition::Applied, completion.finished_at.clone())
                };
                sqlite_complete_run_idempotency(&tx, &run_id, &finished_at)?;
                tx.commit().map_err(|error| {
                    IronCrewError::Validation(format!(
                        "SQLite update completion commit error: {error}"
                    ))
                })?;
                tracing::info!("Run completion saved: {} ({})", run_id, completion.status);
                Ok(transition)
            })
            .await,
        )
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
        let conn = Arc::clone(&self.conn);
        let run_id = run_id.to_string();
        let status = status.to_string();
        let owner_instance_id = self.lease.instance_id().to_string();

        flatten_join(
            tokio::task::spawn_blocking(move || {
                let conn = conn
                    .lock()
                    .map_err(|e| IronCrewError::Validation(format!("SQLite lock error: {}", e)))?;
                let rows = conn
                    .execute(
                        "UPDATE runs SET status = ?1
                         WHERE run_id = ?2 AND status IN ('running', 'waiting_for_input')
                           AND owner_instance_id = ?3",
                        rusqlite::params![status, run_id, owner_instance_id],
                    )
                    .map_err(|e| {
                        IronCrewError::Validation(format!("SQLite update status: {}", e))
                    })?;
                if rows == 0 {
                    return Err(IronCrewError::Validation(format!(
                        "Run '{}' not found or not in an in-flight state",
                        run_id
                    )));
                }
                Ok(())
            })
            .await,
        )
    }

    fn instance_id(&self) -> &str {
        self.lease.instance_id()
    }

    fn run_lease_ttl(&self) -> std::time::Duration {
        self.lease.ttl()
    }

    async fn heartbeat_owned_runs(&self) -> Result<usize> {
        let conn = Arc::clone(&self.conn);
        let owner_instance_id = self.lease.instance_id().to_string();
        let lease_expires_at = self.lease.deadline_now();
        flatten_join(
            tokio::task::spawn_blocking(move || {
                let mut conn = conn
                    .lock()
                    .map_err(|e| IronCrewError::Validation(format!("SQLite lock error: {}", e)))?;
                let tx = conn
                    .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                    .map_err(|error| {
                        IronCrewError::Validation(format!(
                            "SQLite heartbeat transaction error: {error}"
                        ))
                    })?;
                let changed = tx
                    .execute(
                        "UPDATE runs SET lease_expires_at = ?1
                     WHERE owner_instance_id = ?2
                       AND status IN ('running', 'waiting_for_input')
                       AND NOT EXISTS (
                           SELECT 1 FROM idempotency AS idem
                           WHERE idem.operation = ?3 AND idem.resource_id = runs.run_id
                       )",
                        rusqlite::params![&lease_expires_at, &owner_instance_id, RUN_OPERATION],
                    )
                    .map_err(|e| IronCrewError::Validation(format!("SQLite heartbeat: {}", e)))?;
                tx.commit().map_err(|error| {
                    IronCrewError::Validation(format!("SQLite heartbeat commit error: {error}"))
                })?;
                Ok(changed)
            })
            .await,
        )
    }

    async fn health_check(&self) -> Result<()> {
        let conn = Arc::clone(&self.conn);
        flatten_join(
            tokio::task::spawn_blocking(move || {
                let conn = conn
                    .lock()
                    .map_err(|e| IronCrewError::Validation(format!("SQLite lock error: {}", e)))?;
                let value: i64 = conn
                    .query_row("SELECT 1", [], |row| row.get(0))
                    .map_err(|e| {
                        IronCrewError::Validation(format!("SQLite health check: {}", e))
                    })?;
                if value != 1 {
                    return Err(IronCrewError::Validation(
                        "SQLite health check returned an unexpected value".into(),
                    ));
                }
                Ok(())
            })
            .await,
        )
    }

    async fn reconcile_abandoned_runs(&self, now: &str) -> Result<usize> {
        let normalized_now = chrono::DateTime::parse_from_rfc3339(now)
            .map_err(|e| {
                IronCrewError::Validation(format!("Invalid reconciliation timestamp: {}", e))
            })?
            .with_timezone(&chrono::Utc)
            .to_rfc3339();
        let conn = Arc::clone(&self.conn);
        let now = now.to_string();

        flatten_join(
            tokio::task::spawn_blocking(move || {
                let mut conn = conn
                    .lock()
                    .map_err(|e| IronCrewError::Validation(format!("SQLite lock error: {}", e)))?;
                let tx = conn
                    .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                    .map_err(|error| {
                        IronCrewError::Validation(format!(
                            "SQLite reconcile transaction error: {error}"
                        ))
                    })?;
                let inserted = tx
                    .execute(
                        "INSERT OR IGNORE INTO runs (run_id, flow_name, flow, status, started_at, \
                         finished_at, duration_ms, task_results, agent_count, task_count, \
                         total_tokens, cached_tokens, tags, owner_instance_id, lease_expires_at) \
                         SELECT resource_id, scope, scope, 'abandoned', created_at, ?1, 0, '[]', \
                                0, 0, 0, 0, '[]', owner_instance_id, '' \
                         FROM idempotency AS idem \
                         WHERE operation = ?2 AND state = 'claimed' \
                           AND julianday(lease_expires_at) <= julianday(?3) \
                           AND NOT EXISTS (SELECT 1 FROM runs WHERE run_id = idem.resource_id)",
                        rusqlite::params![&now, RUN_OPERATION, &normalized_now],
                    )
                    .map_err(|error| {
                        IronCrewError::Validation(format!(
                            "SQLite idempotent run fallback error: {error}"
                        ))
                    })?;
                let rows = tx
                    .execute(
                        "UPDATE runs
                         SET status = 'abandoned', finished_at = ?1, lease_expires_at = ''
                         WHERE status IN ('running', 'waiting_for_input')
                           AND (lease_expires_at = '' \
                                OR julianday(lease_expires_at) IS NULL \
                                OR julianday(lease_expires_at) <= julianday(?2))",
                        rusqlite::params![&now, &normalized_now],
                    )
                    .map_err(|e| IronCrewError::Validation(format!("SQLite reconcile: {}", e)))?;
                sqlite_complete_terminal_run_idempotency(&tx)?;
                sqlite_reconcile_expired_conversation_idempotency(&tx, &normalized_now)?;
                tx.commit().map_err(|error| {
                    IronCrewError::Validation(format!("SQLite reconcile commit error: {error}"))
                })?;
                Ok(inserted.saturating_add(rows))
            })
            .await,
        )
    }

    async fn get_run(&self, run_id: &str) -> Result<RunRecord> {
        let conn = Arc::clone(&self.conn);
        let run_id = run_id.to_string();

        flatten_join(
            tokio::task::spawn_blocking(move || {
                let conn = conn
                    .lock()
                    .map_err(|e| IronCrewError::Validation(format!("SQLite lock error: {}", e)))?;

                let mut stmt = conn
                    .prepare(
                        "SELECT run_id, flow_name, flow, status, started_at, finished_at, duration_ms, task_results, agent_count, task_count, total_tokens, cached_tokens, tags, owner_instance_id, lease_expires_at FROM runs WHERE run_id = ?1",
                    )
                    .map_err(|e| IronCrewError::Validation(format!("SQLite prepare error: {}", e)))?;

                let record = stmt
                    .query_row(rusqlite::params![run_id], |row| {
                        let status_str: String = row.get(3)?;
                        let task_results_json: String = row.get(7)?;
                        let tags_json: String = row.get(12)?;

                        Ok((
                            RunRecord {
                                run_id: row.get(0)?,
                                flow_name: row.get(1)?,
                                flow: row.get(2)?,
                                // Placeholder — replaced below after decoding.
                                status: RunStatus::Running,
                                started_at: row.get(4)?,
                                finished_at: row.get(5)?,
                                duration_ms: row.get::<_, i64>(6)? as u64,
                                task_results: decode_stored_json(&task_results_json, 7)?,
                                agent_count: row.get::<_, i64>(8)? as usize,
                                task_count: row.get::<_, i64>(9)? as usize,
                                total_tokens: row.get::<_, i64>(10)? as u32,
                                cached_tokens: row.get::<_, i64>(11)? as u32,
                                tags: decode_stored_json(&tags_json, 12)?,
                                owner_instance_id: row.get(13)?,
                                lease_expires_at: row.get(14)?,
                            },
                            status_str,
                        ))
                    })
                    .map_err(|e| match e {
                        rusqlite::Error::QueryReturnedNoRows => {
                            IronCrewError::Validation(format!("Run '{}' not found", run_id))
                        }
                        _ => IronCrewError::Validation(format!("SQLite query error: {}", e)),
                    })?;

                let (mut record, status_str) = record;
                record.status = status_str.parse::<RunStatus>()?;
                Ok(record)
            })
            .await,
        )
    }

    async fn list_runs_summary(
        &self,
        filter: &ListRunsFilter,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<RunSummary>> {
        let conn = Arc::clone(&self.conn);
        let wc = store_sql::runs_where(filter, Dialect::Sqlite);

        flatten_join(
            tokio::task::spawn_blocking(move || {
                let conn = conn
                    .lock()
                    .map_err(|e| IronCrewError::Validation(format!("SQLite lock error: {}", e)))?;

                // NOTE: we never select task_results. LIMIT/OFFSET are trusted
                // integer literals so the builder's placeholder numbering is
                // undisturbed.
                let mut sql = format!(
                    "SELECT run_id, flow_name, flow, status, started_at, finished_at, duration_ms, \
                     agent_count, task_count, total_tokens, cached_tokens, tags \
                     FROM runs{}",
                    wc.sql
                );
                sql.push_str(" ORDER BY started_at DESC");
                if limit > 0 {
                    sql.push_str(&format!(" LIMIT {}", limit as i64));
                    if offset > 0 {
                        sql.push_str(&format!(" OFFSET {}", offset as i64));
                    }
                }

                let boxed = to_sql_params(wc.params);
                let refs: Vec<&dyn rusqlite::types::ToSql> =
                    boxed.iter().map(|b| b.as_ref()).collect();

                let mut stmt = conn.prepare(&sql).map_err(|e| {
                    IronCrewError::Validation(format!("SQLite prepare error: {}", e))
                })?;

                let rows = stmt
                    .query_map(rusqlite::params_from_iter(refs), |row| {
                        let status_str: String = row.get(3)?;
                        let tags_json: String = row.get(11)?;
                        Ok((
                            RunSummary {
                                run_id: row.get(0)?,
                                flow_name: row.get(1)?,
                                flow: row.get(2)?,
                                // Placeholder — replaced below after decoding.
                                status: RunStatus::Running,
                                started_at: row.get(4)?,
                                finished_at: row.get(5)?,
                                duration_ms: row.get::<_, i64>(6)? as u64,
                                agent_count: row.get::<_, i64>(7)? as usize,
                                task_count: row.get::<_, i64>(8)? as usize,
                                total_tokens: row.get::<_, i64>(9)? as u32,
                                cached_tokens: row.get::<_, i64>(10)? as u32,
                                tags: decode_stored_json(&tags_json, 11)?,
                            },
                            status_str,
                        ))
                    })
                    .map_err(|e| IronCrewError::Validation(format!("SQLite query error: {}", e)))?;

                let mut summaries = Vec::new();
                for row in rows {
                    let (mut summary, status_str) = row.map_err(|e| {
                        IronCrewError::Validation(format!("SQLite row error: {}", e))
                    })?;
                    summary.status = status_str.parse::<RunStatus>()?;
                    summaries.push(summary);
                }
                Ok(summaries)
            })
            .await,
        )
    }

    async fn count_runs(&self, filter: &ListRunsFilter) -> Result<u64> {
        let conn = Arc::clone(&self.conn);
        let wc = store_sql::runs_where(filter, Dialect::Sqlite);

        flatten_join(
            tokio::task::spawn_blocking(move || {
                let conn = conn
                    .lock()
                    .map_err(|e| IronCrewError::Validation(format!("SQLite lock error: {}", e)))?;

                let sql = format!("SELECT COUNT(*) FROM runs{}", wc.sql);
                let boxed = to_sql_params(wc.params);
                let refs: Vec<&dyn rusqlite::types::ToSql> =
                    boxed.iter().map(|b| b.as_ref()).collect();

                let count: i64 = conn
                    .query_row(&sql, rusqlite::params_from_iter(refs), |row| row.get(0))
                    .map_err(|e| IronCrewError::Validation(format!("SQLite count error: {}", e)))?;
                Ok(count as u64)
            })
            .await,
        )
    }

    async fn delete_run(&self, run_id: &str) -> Result<()> {
        let conn = Arc::clone(&self.conn);
        let run_id = run_id.to_string();

        flatten_join(
            tokio::task::spawn_blocking(move || {
                let conn = conn
                    .lock()
                    .map_err(|e| IronCrewError::Validation(format!("SQLite lock error: {}", e)))?;

                let affected = conn
                    .execute(
                        "DELETE FROM runs WHERE run_id = ?1",
                        rusqlite::params![run_id],
                    )
                    .map_err(|e| {
                        IronCrewError::Validation(format!("SQLite delete error: {}", e))
                    })?;

                if affected == 0 {
                    return Err(IronCrewError::Validation(format!(
                        "Run '{}' not found",
                        run_id
                    )));
                }
                Ok(())
            })
            .await,
        )
    }

    // ─── Persistent sessions ────────────────────────────────────────────────

    async fn lookup_idempotency_for_principal(
        &self,
        principal_id: &PrincipalId,
        key_hash: &str,
        request_fingerprint: &str,
        now: &str,
    ) -> Result<IdempotencyLookup> {
        validate_digest("idempotency key hash", key_hash)?;
        validate_digest("request fingerprint", request_fingerprint)?;
        sqlite_parse_idempotency_time("idempotency current time", now)?;
        let conn = Arc::clone(&self.conn);
        let key_hash = key_hash.to_string();
        let principal_id = principal_id.clone();
        let fingerprint = request_fingerprint.to_string();
        let now = now.to_string();
        flatten_join(
            tokio::task::spawn_blocking(move || {
                let conn = conn.lock().map_err(|error| {
                    IronCrewError::Validation(format!("SQLite lock error: {error}"))
                })?;
                let Some(record) = sqlite_idempotency_record(&conn, &key_hash)? else {
                    return Ok(IdempotencyLookup::Miss);
                };
                if record.principal_id != principal_id || record.request_fingerprint != fingerprint
                {
                    return Ok(IdempotencyLookup::Conflict);
                }
                if record.state.is_terminal()
                    && match record.expires_at.as_deref() {
                        Some(expires) => sqlite_deadline_passed(expires, &now)?,
                        None => false,
                    }
                {
                    return Ok(IdempotencyLookup::Miss);
                }
                if record.state == IdempotencyState::Indeterminate {
                    return Ok(IdempotencyLookup::Indeterminate(record));
                }
                if record.replayable() {
                    return Ok(IdempotencyLookup::Replay(record));
                }
                if record.state.is_in_flight()
                    && sqlite_deadline_passed(&record.lease_expires_at, &now)?
                {
                    return Ok(IdempotencyLookup::Indeterminate(record));
                }
                if record.state.is_in_flight() {
                    return Ok(IdempotencyLookup::InProgress(record));
                }
                Ok(IdempotencyLookup::Indeterminate(record))
            })
            .await,
        )
    }

    async fn claim_idempotency_with_limits(
        &self,
        claim: IdempotencyClaim,
        limits: IdempotencyLimits,
    ) -> Result<IdempotencyClaimOutcome> {
        claim.validate()?;
        limits.validate()?;
        let recovery_grace_ttl = self.lease.ttl();
        let conn = Arc::clone(&self.conn);
        flatten_join(
            tokio::task::spawn_blocking(move || {
                let mut conn = conn.lock().map_err(|error| {
                    IronCrewError::Validation(format!("SQLite lock error: {error}"))
                })?;
                let tx = conn
                    .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                    .map_err(|error| {
                        IronCrewError::Validation(format!(
                            "SQLite idempotency claim transaction error: {error}"
                        ))
                    })?;
                sqlite_prune_idempotency(&tx, &claim.created_at, limits.prune_batch)?;

                if let Some(mut existing) = sqlite_idempotency_record(&tx, &claim.key_hash)? {
                    let expired_terminal = existing.state.is_terminal()
                        && match existing.expires_at.as_deref() {
                            Some(expires) => sqlite_deadline_passed(expires, &claim.created_at)?,
                            None => false,
                        };
                    if expired_terminal {
                        tx.execute(
                            "DELETE FROM idempotency WHERE key_hash = ?1",
                            rusqlite::params![&claim.key_hash],
                        )
                        .map_err(|error| {
                            IronCrewError::Validation(format!(
                                "SQLite expired idempotency delete error: {error}"
                            ))
                        })?;
                    } else {
                        let outcome = if existing.principal_id != claim.principal_id
                            || existing.request_fingerprint != claim.request_fingerprint
                        {
                            IdempotencyClaimOutcome::Conflict
                        } else if existing.state == IdempotencyState::Indeterminate {
                            IdempotencyClaimOutcome::Indeterminate(existing)
                        } else if existing.replayable() {
                            IdempotencyClaimOutcome::Replay(existing)
                        } else if existing.state.is_in_flight()
                            && sqlite_deadline_passed(
                                &existing.lease_expires_at,
                                &claim.created_at,
                            )?
                        {
                            let expires_at =
                                sqlite_retention_expiry(&claim.created_at, existing.ttl_seconds)?;
                            sqlite_write_indeterminate(
                                &tx,
                                &mut existing,
                                &claim.created_at,
                                &expires_at,
                            )?;
                            IdempotencyClaimOutcome::Indeterminate(existing)
                        } else if existing.state.is_in_flight() {
                            IdempotencyClaimOutcome::InProgress(existing)
                        } else {
                            IdempotencyClaimOutcome::Indeterminate(existing)
                        };
                        tx.commit().map_err(|error| {
                            IronCrewError::Validation(format!(
                                "SQLite idempotency claim commit error: {error}"
                            ))
                        })?;
                        return Ok(outcome);
                    }
                }

                let mut recovery_hazard_key = None;
                if let Some(exclusive_scope) = claim.exclusive_scope.as_deref() {
                    if let Some(mut existing) =
                        sqlite_active_exclusive_record(&tx, exclusive_scope)?
                    {
                        if sqlite_deadline_passed(&existing.lease_expires_at, &claim.created_at)? {
                            let expires_at =
                                sqlite_retention_expiry(&claim.created_at, existing.ttl_seconds)?;
                            sqlite_write_indeterminate(
                                &tx,
                                &mut existing,
                                &claim.created_at,
                                &expires_at,
                            )?;
                            tx.commit().map_err(|error| {
                                IronCrewError::Validation(format!(
                                    "SQLite expired idempotency barrier commit error: {error}"
                                ))
                            })?;
                            return Ok(IdempotencyClaimOutcome::Busy);
                        }
                        tx.commit().map_err(|error| {
                            IronCrewError::Validation(format!(
                                "SQLite idempotency busy commit error: {error}"
                            ))
                        })?;
                        return Ok(IdempotencyClaimOutcome::Busy);
                    }

                    let hazards = sqlite_indeterminate_exclusive_records(&tx, exclusive_scope)?;
                    if !hazards.is_empty() {
                        let acknowledged = hazards.len() == 1
                            && hazards[0].principal_id == claim.principal_id
                            && claim.recovery_key_hash.as_deref()
                                == Some(hazards[0].key_hash.as_str());
                        if !acknowledged
                            || !sqlite_recovery_grace_elapsed(
                                &hazards[0],
                                &claim.created_at,
                                recovery_grace_ttl,
                            )?
                        {
                            tx.commit().map_err(|error| {
                                IronCrewError::Validation(format!(
                                    "SQLite idempotency hazard barrier commit error: {error}"
                                ))
                            })?;
                            return Ok(IdempotencyClaimOutcome::Busy);
                        }
                        recovery_hazard_key = Some(hazards[0].key_hash.clone());
                    }
                }

                if claim.operation == CONVERSATION_MESSAGE_OPERATION {
                    validate_session_id(&claim.resource_id)?;
                    let expected_revision = claim.base_revision.ok_or_else(|| {
                        IronCrewError::Validation(
                            "Conversation idempotency claim has no base revision".into(),
                        )
                    })?;
                    let current = match tx.query_row(
                        "SELECT revision, \
                                CASE WHEN length(CAST(execution AS BLOB)) <= ?3 THEN execution END, \
                                length(CAST(execution AS BLOB)) \
                         FROM conversations \
                         WHERE id = ?1 AND flow_path IS ?2",
                        rusqlite::params![
                            &claim.resource_id,
                            &claim.scope,
                            i64::try_from(HARD_STORED_CONVERSATION_EXECUTION_BYTES)
                                .unwrap_or(i64::MAX),
                        ],
                        |row| {
                            Ok((
                                row.get::<_, i64>(0)?,
                                row.get::<_, Option<String>>(1)?,
                                row.get::<_, i64>(2)?,
                            ))
                        },
                    ) {
                        Ok((revision, execution, execution_bytes)) => Some((
                            u64::try_from(revision).map_err(|_| {
                                IronCrewError::Validation(
                                    "SQLite conversation revision is negative".into(),
                                )
                            })?,
                            sqlite_bounded_conversation_execution(
                                execution,
                                execution_bytes,
                                1,
                            )?,
                        )),
                        Err(rusqlite::Error::QueryReturnedNoRows) => None,
                        Err(error) => {
                            return Err(IronCrewError::Validation(format!(
                                "SQLite conversation idempotency revision query error: {error}"
                            )));
                        }
                    };
                    let valid = current.as_ref().is_some_and(|(revision, execution)| {
                        let expected_scope = super::sessions::conversation_mutation_scope(
                            &claim.scope,
                            &claim.resource_id,
                            &execution.incarnation_id,
                        );
                        execution.validate().is_ok()
                            && *revision == expected_revision
                            && claim.exclusive_scope.as_deref() == Some(expected_scope.as_str())
                    });
                    if !valid {
                        tx.commit().map_err(|error| {
                            IronCrewError::Validation(format!(
                                "SQLite conversation idempotency conflict commit error: {error}"
                            ))
                        })?;
                        return Ok(IdempotencyClaimOutcome::Conflict);
                    }
                }

                let (count, stored_response_bytes): (i64, i64) = tx
                    .query_row(
                        "SELECT COUNT(*), \
                                COALESCE(SUM(length(CAST(response_body AS BLOB))), 0) \
                         FROM idempotency",
                        [],
                        |row| Ok((row.get(0)?, row.get(1)?)),
                    )
                    .map_err(|error| {
                        IronCrewError::Validation(format!(
                            "SQLite idempotency claim aggregate error: {error}"
                        ))
                    })?;
                let (principal_count, principal_in_flight, principal_response_bytes):
                    (i64, i64, i64) = tx
                    .query_row(
                        "SELECT COUNT(*), \
                                COALESCE(SUM(CASE WHEN state IN ('claimed', 'running') THEN 1 ELSE 0 END), 0), \
                                COALESCE(SUM(length(CAST(response_body AS BLOB))), 0) \
                         FROM idempotency WHERE principal_id = ?1",
                        rusqlite::params![claim.principal_id.as_str()],
                        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                    )
                    .map_err(|error| {
                        IronCrewError::Validation(format!(
                            "SQLite principal idempotency aggregate error: {error}"
                        ))
                    })?;
                if usize::try_from(count).unwrap_or(usize::MAX) >= limits.global_max_records {
                    let outcome = IdempotencyClaimOutcome::QuotaExceeded {
                        scope: IdempotencyQuotaScope::Global,
                        resource: IdempotencyQuotaResource::Records,
                        retry_after_seconds: sqlite_quota_retry_after(
                            &tx,
                            None,
                            &claim.created_at,
                        )?,
                    };
                    // Pruning happens in this transaction. Commit it even
                    // when the current claim is denied so a bounded backlog
                    // makes forward progress across retries.
                    tx.commit().map_err(|error| {
                        IronCrewError::Validation(format!(
                            "SQLite global idempotency quota commit error: {error}"
                        ))
                    })?;
                    return Ok(outcome);
                }
                if usize::try_from(principal_count).unwrap_or(usize::MAX)
                    >= limits.principal_max_records
                {
                    let outcome = IdempotencyClaimOutcome::QuotaExceeded {
                        scope: IdempotencyQuotaScope::Principal,
                        resource: IdempotencyQuotaResource::Records,
                        retry_after_seconds: sqlite_quota_retry_after(
                            &tx,
                            Some(&claim.principal_id),
                            &claim.created_at,
                        )?,
                    };
                    tx.commit().map_err(|error| {
                        IronCrewError::Validation(format!(
                            "SQLite principal idempotency quota commit error: {error}"
                        ))
                    })?;
                    return Ok(outcome);
                }
                if usize::try_from(principal_in_flight).unwrap_or(usize::MAX)
                    >= limits.principal_max_in_flight
                {
                    let outcome = IdempotencyClaimOutcome::QuotaExceeded {
                        scope: IdempotencyQuotaScope::Principal,
                        resource: IdempotencyQuotaResource::InFlight,
                        retry_after_seconds: sqlite_quota_retry_after(
                            &tx,
                            Some(&claim.principal_id),
                            &claim.created_at,
                        )?,
                    };
                    tx.commit().map_err(|error| {
                        IronCrewError::Validation(format!(
                            "SQLite principal in-flight quota commit error: {error}"
                        ))
                    })?;
                    return Ok(outcome);
                }

                let stored_response_bytes =
                    usize::try_from(stored_response_bytes).map_err(|_| {
                        IronCrewError::Validation(
                            "Idempotency response byte total is out of range".into(),
                        )
                    })?;
                let principal_response_bytes =
                    usize::try_from(principal_response_bytes).map_err(|_| {
                        IronCrewError::Validation(
                            "Principal idempotency response byte total is out of range".into(),
                        )
                    })?;
                let mut record = claim.to_record();
                record.response_body = record.response_body.filter(|body| {
                    let global_fits = stored_response_bytes
                        .checked_add(body.len())
                        .is_some_and(|total| total <= limits.global_max_response_bytes);
                    let principal_fits = principal_response_bytes
                        .checked_add(body.len())
                        .is_some_and(|total| total <= limits.principal_max_response_bytes);
                    global_fits && principal_fits
                });
                record.validate()?;
                if let Some(hazard_key) = recovery_hazard_key.as_deref() {
                    let changed = tx
                        .execute(
                            "UPDATE idempotency SET exclusive_scope = NULL, updated_at = ?1 \
                             WHERE key_hash = ?2 AND exclusive_scope = ?3 \
                               AND state = 'indeterminate'",
                            rusqlite::params![
                                &claim.created_at,
                                hazard_key,
                                claim.exclusive_scope.as_deref(),
                            ],
                        )
                        .map_err(|error| {
                            IronCrewError::Validation(format!(
                                "SQLite idempotency hazard recovery error: {error}"
                            ))
                        })?;
                    if changed != 1 {
                        return Err(IronCrewError::Conflict(
                            "Idempotency recovery hazard changed before claim insertion".into(),
                        ));
                    }
                }
                tx.execute(
                    "INSERT INTO idempotency (key_hash, principal_id, request_fingerprint, operation, scope, \
                     resource_id, exclusive_scope, attempt_id, owner_instance_id, base_revision, \
                     state, response_status, response_body, lease_expires_at, created_at, \
                     updated_at, completed_at, expires_at, ttl_seconds) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, \
                             ?15, ?16, ?17, ?18, ?19)",
                    rusqlite::params![
                        &record.key_hash,
                        record.principal_id.as_str(),
                        &record.request_fingerprint,
                        &record.operation,
                        &record.scope,
                        &record.resource_id,
                        &record.exclusive_scope,
                        &record.attempt_id,
                        &record.owner_instance_id,
                        record
                            .base_revision
                            .map(i64::try_from)
                            .transpose()
                            .map_err(|_| {
                                IronCrewError::Validation(
                                    "Idempotency base revision is out of range".into(),
                                )
                            })?,
                        record.state.to_string(),
                        record.response_status.map(i64::from),
                        &record.response_body,
                        &record.lease_expires_at,
                        &record.created_at,
                        &record.updated_at,
                        &record.completed_at,
                        &record.expires_at,
                        i64::try_from(record.ttl_seconds).map_err(|_| {
                            IronCrewError::Validation("Idempotency TTL is out of range".into())
                        })?,
                    ],
                )
                .map_err(|error| {
                    IronCrewError::Validation(format!(
                        "SQLite idempotency claim insert error: {error}"
                    ))
                })?;
                tx.commit().map_err(|error| {
                    IronCrewError::Validation(format!(
                        "SQLite idempotency claim commit error: {error}"
                    ))
                })?;
                Ok(IdempotencyClaimOutcome::Claimed(record))
            })
            .await,
        )
    }

    async fn heartbeat_idempotency(
        &self,
        key_hash: &str,
        attempt_id: &str,
        new_lease_expires_at: &str,
    ) -> Result<bool> {
        validate_digest("idempotency key hash", key_hash)?;
        sqlite_parse_idempotency_time("idempotency lease expiry", new_lease_expires_at)?;
        let conn = Arc::clone(&self.conn);
        let key_hash = key_hash.to_string();
        let attempt_id = attempt_id.to_string();
        let lease = new_lease_expires_at.to_string();
        flatten_join(
            tokio::task::spawn_blocking(move || {
                let conn = conn.lock().map_err(|error| {
                    IronCrewError::Validation(format!("SQLite lock error: {error}"))
                })?;
                let Some(record) = sqlite_idempotency_record(&conn, &key_hash)? else {
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
                let changed = conn
                    .execute(
                        "UPDATE idempotency SET lease_expires_at = ?1 \
                         WHERE key_hash = ?2 AND attempt_id = ?3 \
                           AND state IN ('claimed', 'running')",
                        rusqlite::params![lease, key_hash, attempt_id],
                    )
                    .map_err(|error| {
                        IronCrewError::Validation(format!(
                            "SQLite idempotency heartbeat error: {error}"
                        ))
                    })?;
                Ok(changed == 1)
            })
            .await,
        )
    }

    async fn heartbeat_idempotent_run(
        &self,
        run_id: &str,
        key_hash: &str,
        attempt_id: &str,
        new_lease_expires_at: &str,
    ) -> Result<RunFenceHeartbeat> {
        validate_run_id(run_id)?;
        validate_digest("idempotency key hash", key_hash)?;
        sqlite_parse_idempotency_time("idempotency run lease expiry", new_lease_expires_at)?;
        let conn = Arc::clone(&self.conn);
        let run_id = run_id.to_string();
        let key_hash = key_hash.to_string();
        let attempt_id = attempt_id.to_string();
        let owner_instance_id = self.lease.instance_id().to_string();
        let lease_expires_at = new_lease_expires_at.to_string();
        flatten_join(
            tokio::task::spawn_blocking(move || {
                let mut conn = conn.lock().map_err(|error| {
                    IronCrewError::Validation(format!("SQLite lock error: {error}"))
                })?;
                let tx = conn
                    .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                    .map_err(|error| {
                        IronCrewError::Validation(format!(
                            "SQLite idempotent run heartbeat transaction error: {error}"
                        ))
                    })?;
                let Some(ledger) = sqlite_idempotency_record(&tx, &key_hash)? else {
                    tx.commit().map_err(|error| {
                        IronCrewError::Validation(format!(
                            "SQLite missing run heartbeat commit error: {error}"
                        ))
                    })?;
                    return Ok(RunFenceHeartbeat::Lost);
                };
                if ledger.attempt_id != attempt_id {
                    return Err(IronCrewError::Conflict(
                        "Idempotency attempt changed before run heartbeat".into(),
                    ));
                }
                if ledger.operation != RUN_OPERATION
                    || ledger.resource_id != run_id
                    || ledger.owner_instance_id != owner_instance_id
                {
                    tx.commit().map_err(|error| {
                        IronCrewError::Validation(format!(
                            "SQLite mismatched run heartbeat commit error: {error}"
                        ))
                    })?;
                    return Ok(RunFenceHeartbeat::Lost);
                }

                let run = match tx.query_row(
                    "SELECT status, owner_instance_id FROM runs WHERE run_id = ?1",
                    rusqlite::params![&run_id],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                ) {
                    Ok((status, owner)) => Some((status.parse::<RunStatus>()?, owner)),
                    Err(rusqlite::Error::QueryReturnedNoRows) => None,
                    Err(error) => {
                        return Err(IronCrewError::Validation(format!(
                            "SQLite idempotent run heartbeat query error: {error}"
                        )));
                    }
                };

                let outcome = match run {
                    None if ledger.state == IdempotencyState::Claimed => {
                        let changed = tx
                            .execute(
                                "UPDATE idempotency SET lease_expires_at = ?1 \
                                 WHERE key_hash = ?2 AND operation = ?3 AND resource_id = ?4 \
                                   AND owner_instance_id = ?5 AND attempt_id = ?6 \
                                   AND state = 'claimed'",
                                rusqlite::params![
                                    &lease_expires_at,
                                    &key_hash,
                                    RUN_OPERATION,
                                    &run_id,
                                    &owner_instance_id,
                                    &attempt_id,
                                ],
                            )
                            .map_err(|error| {
                                IronCrewError::Validation(format!(
                                    "SQLite claimed run heartbeat error: {error}"
                                ))
                            })?;
                        if changed == 1 {
                            RunFenceHeartbeat::Owned
                        } else {
                            RunFenceHeartbeat::Lost
                        }
                    }
                    None => RunFenceHeartbeat::Lost,
                    Some((_, run_owner)) if run_owner != ledger.owner_instance_id => {
                        RunFenceHeartbeat::Lost
                    }
                    Some((status, _)) if status.is_terminal() => {
                        if ledger.state == IdempotencyState::Indeterminate {
                            RunFenceHeartbeat::Lost
                        } else {
                            RunFenceHeartbeat::Terminal(status)
                        }
                    }
                    Some(_) if ledger.state != IdempotencyState::Running => RunFenceHeartbeat::Lost,
                    Some(_) => {
                        let run_changed = tx
                            .execute(
                                "UPDATE runs SET lease_expires_at = ?1 \
                                 WHERE run_id = ?2 AND owner_instance_id = ?3 \
                                   AND status IN ('running', 'waiting_for_input')",
                                rusqlite::params![&lease_expires_at, &run_id, &owner_instance_id,],
                            )
                            .map_err(|error| {
                                IronCrewError::Validation(format!(
                                    "SQLite idempotent run lease heartbeat error: {error}"
                                ))
                            })?;
                        let ledger_changed = tx
                            .execute(
                                "UPDATE idempotency SET lease_expires_at = ?1 \
                                 WHERE key_hash = ?2 AND operation = ?3 AND resource_id = ?4 \
                                   AND owner_instance_id = ?5 AND attempt_id = ?6 \
                                   AND state = 'running'",
                                rusqlite::params![
                                    &lease_expires_at,
                                    &key_hash,
                                    RUN_OPERATION,
                                    &run_id,
                                    &owner_instance_id,
                                    &attempt_id,
                                ],
                            )
                            .map_err(|error| {
                                IronCrewError::Validation(format!(
                                    "SQLite idempotent run ledger heartbeat error: {error}"
                                ))
                            })?;
                        if run_changed == 1 && ledger_changed == 1 {
                            RunFenceHeartbeat::Owned
                        } else {
                            return Err(IronCrewError::Conflict(
                                "Run fence changed during idempotent heartbeat".into(),
                            ));
                        }
                    }
                };
                tx.commit().map_err(|error| {
                    IronCrewError::Validation(format!(
                        "SQLite idempotent run heartbeat commit error: {error}"
                    ))
                })?;
                Ok(outcome)
            })
            .await,
        )
    }

    async fn complete_idempotency_with_limits(
        &self,
        completion: IdempotencyCompletion,
        limits: IdempotencyLimits,
    ) -> Result<IdempotencyCompletionOutcome> {
        completion.validate()?;
        limits.validate()?;
        let conn = Arc::clone(&self.conn);
        flatten_join(
            tokio::task::spawn_blocking(move || {
                let mut conn = conn.lock().map_err(|error| {
                    IronCrewError::Validation(format!("SQLite lock error: {error}"))
                })?;
                let tx = conn
                    .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                    .map_err(|error| {
                        IronCrewError::Validation(format!(
                            "SQLite idempotency completion transaction error: {error}"
                        ))
                    })?;
                let Some(record) = sqlite_idempotency_record(&tx, &completion.key_hash)? else {
                    return Err(IronCrewError::Validation(
                        "Idempotency claim not found during completion".into(),
                    ));
                };
                sqlite_completion_fence(&record, &completion)?;
                if record.state == IdempotencyState::Completed {
                    return Ok(IdempotencyCompletionOutcome {
                        replayable: record.replayable(),
                        already_completed: true,
                    });
                }
                if record.state == IdempotencyState::Indeterminate {
                    return Err(IronCrewError::Conflict(
                        "Indeterminate idempotency outcomes cannot be completed".into(),
                    ));
                }
                let (stored_bytes, principal_stored_bytes) =
                    sqlite_response_bytes(&tx, &completion.principal_id, &completion.key_hash)?;
                let response_body = completion.response_body.filter(|body| {
                    let global_fits = stored_bytes
                        .checked_add(body.len())
                        .is_some_and(|total| total <= limits.global_max_response_bytes);
                    let principal_fits = principal_stored_bytes
                        .checked_add(body.len())
                        .is_some_and(|total| total <= limits.principal_max_response_bytes);
                    global_fits && principal_fits
                });
                let changed = tx
                    .execute(
                        "UPDATE idempotency SET state = 'completed', response_status = ?1, \
                         response_body = ?2, updated_at = ?3, completed_at = ?3, expires_at = ?4 \
                         WHERE key_hash = ?5 AND attempt_id = ?6 \
                           AND state IN ('claimed', 'running')",
                        rusqlite::params![
                            i64::from(completion.response_status),
                            &response_body,
                            &completion.completed_at,
                            &completion.expires_at,
                            &completion.key_hash,
                            &completion.attempt_id,
                        ],
                    )
                    .map_err(|error| {
                        IronCrewError::Validation(format!(
                            "SQLite idempotency completion update error: {error}"
                        ))
                    })?;
                if changed != 1 {
                    return Err(IronCrewError::Conflict(
                        "Idempotency operation changed during completion".into(),
                    ));
                }
                tx.commit().map_err(|error| {
                    IronCrewError::Validation(format!(
                        "SQLite idempotency completion commit error: {error}"
                    ))
                })?;
                Ok(IdempotencyCompletionOutcome {
                    replayable: response_body.is_some(),
                    already_completed: false,
                })
            })
            .await,
        )
    }

    async fn commit_conversation_idempotency_with_limits(
        &self,
        completion: IdempotencyCompletion,
        conversation: &ConversationRecord,
        limits: IdempotencyLimits,
    ) -> Result<ConversationIdempotencyCommit> {
        completion.validate()?;
        limits.validate()?;
        let conn = Arc::clone(&self.conn);
        let conversation = conversation.clone();
        flatten_join(
            tokio::task::spawn_blocking(move || {
                validate_conversation_record_for_write(&conversation)?;
                let mut conn = conn
                    .lock()
                    .map_err(|error| IronCrewError::Validation(format!("SQLite lock error: {error}")))?;
                let tx = conn
                    .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                    .map_err(|error| {
                        IronCrewError::Validation(format!(
                            "SQLite conversation idempotency transaction error: {error}"
                        ))
                    })?;
                let Some(record) = sqlite_idempotency_record(&tx, &completion.key_hash)? else {
                    return Err(IronCrewError::Validation(
                        "Idempotency claim not found during conversation commit".into(),
                    ));
                };
                sqlite_completion_fence(&record, &completion)?;
                let expected_scope = super::sessions::conversation_mutation_scope(
                    conversation.flow_path.as_deref().unwrap_or(""),
                    &conversation.id,
                    &conversation.execution.incarnation_id,
                );
                if record.operation != CONVERSATION_MESSAGE_OPERATION
                    || record.resource_id != conversation.id
                    || record.scope != conversation.flow_path.as_deref().unwrap_or("")
                    || record.exclusive_scope.as_deref() != Some(expected_scope.as_str())
                {
                    return Err(IronCrewError::Conflict(
                        "Idempotency claim does not match the conversation scope".into(),
                    ));
                }
                let expected_revision = record.base_revision.ok_or_else(|| {
                    IronCrewError::Validation(
                        "Conversation idempotency claim has no base revision".into(),
                    )
                })?;
                if expected_revision != conversation.revision {
                    return Err(IronCrewError::Conflict(format!(
                        "Conversation '{}' changed before idempotent commit",
                        conversation.id
                    )));
                }
                if record.state == IdempotencyState::Completed {
                    return Ok(ConversationIdempotencyCommit {
                        revision: expected_revision.saturating_add(1),
                        replayable: record.replayable(),
                        already_completed: true,
                    });
                }
                if record.state == IdempotencyState::Indeterminate {
                    return Err(IronCrewError::Conflict(
                        "Indeterminate conversation outcomes cannot be committed".into(),
                    ));
                }

                let current = match tx.query_row(
                    "SELECT revision, \
                            CASE WHEN length(CAST(execution AS BLOB)) <= ?3 THEN execution END, \
                            length(CAST(execution AS BLOB)) \
                     FROM conversations WHERE id = ?1 AND flow_path IS ?2",
                    rusqlite::params![
                        &conversation.id,
                        &conversation.flow_path,
                        i64::try_from(HARD_STORED_CONVERSATION_EXECUTION_BYTES)
                            .unwrap_or(i64::MAX),
                    ],
                    |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, Option<String>>(1)?,
                            row.get::<_, i64>(2)?,
                        ))
                    },
                ) {
                    Ok((revision, execution, execution_bytes)) => Some((
                        u64::try_from(revision).map_err(|_| {
                            IronCrewError::Validation(
                                "SQLite conversation revision is negative".into(),
                            )
                        })?,
                        sqlite_bounded_conversation_execution(execution, execution_bytes, 1)?,
                    )),
                    Err(rusqlite::Error::QueryReturnedNoRows) => None,
                    Err(error) => {
                        return Err(IronCrewError::Validation(format!(
                            "SQLite conversation revision query error: {error}"
                        )));
                    }
                };
                if !current.as_ref().is_some_and(|(revision, execution)| {
                    *revision == expected_revision && execution == &conversation.execution
                }) {
                    return Err(IronCrewError::Conflict(format!(
                        "Conversation '{}' changed since revision {expected_revision}",
                        conversation.id
                    )));
                }
                let next_revision = expected_revision.checked_add(1).ok_or_else(|| {
                    IronCrewError::Validation("Conversation revision overflow".into())
                })?;
                let messages = serialize_conversation_messages(&conversation.messages)?;
                let execution = serialize_conversation_execution(&conversation.execution)?;
                let changed = tx
                    .execute(
                        "UPDATE conversations SET flow_name = ?3, agent_name = ?4, execution = ?5, messages = ?6, \
                         created_at = ?7, updated_at = ?8, revision = ?9 \
                         WHERE id = ?1 AND flow_path IS ?2 AND revision = ?10",
                        rusqlite::params![
                            &conversation.id,
                            &conversation.flow_path,
                            &conversation.flow_name,
                            &conversation.agent_name,
                            &execution,
                            &messages,
                            &conversation.created_at,
                            &conversation.updated_at,
                            i64::try_from(next_revision).map_err(|_| {
                                IronCrewError::Validation(
                                    "Conversation revision is out of range".into(),
                                )
                            })?,
                            i64::try_from(expected_revision).map_err(|_| {
                                IronCrewError::Validation(
                                    "Conversation revision is out of range".into(),
                                )
                            })?,
                        ],
                    )
                    .map_err(|error| {
                        IronCrewError::Validation(format!(
                            "SQLite conversation idempotency update error: {error}"
                        ))
                    })?;
                if changed != 1 {
                    return Err(IronCrewError::Conflict(format!(
                        "Conversation '{}' changed during idempotent commit",
                        conversation.id
                    )));
                }

                let (stored_bytes, principal_stored_bytes) = sqlite_response_bytes(
                    &tx,
                    &completion.principal_id,
                    &completion.key_hash,
                )?;
                let response_body = completion.response_body.filter(|body| {
                    let global_fits = stored_bytes
                        .checked_add(body.len())
                        .is_some_and(|total| total <= limits.global_max_response_bytes);
                    let principal_fits = principal_stored_bytes
                        .checked_add(body.len())
                        .is_some_and(|total| total <= limits.principal_max_response_bytes);
                    global_fits && principal_fits
                });
                let changed = tx
                    .execute(
                        "UPDATE idempotency SET state = 'completed', response_status = ?1, \
                         response_body = ?2, updated_at = ?3, completed_at = ?3, expires_at = ?4 \
                         WHERE key_hash = ?5 AND attempt_id = ?6 \
                           AND state IN ('claimed', 'running')",
                        rusqlite::params![
                            i64::from(completion.response_status),
                            &response_body,
                            &completion.completed_at,
                            &completion.expires_at,
                            &completion.key_hash,
                            &completion.attempt_id,
                        ],
                    )
                    .map_err(|error| {
                        IronCrewError::Validation(format!(
                            "SQLite conversation idempotency completion error: {error}"
                        ))
                    })?;
                if changed != 1 {
                    return Err(IronCrewError::Conflict(
                        "Idempotency operation changed during conversation commit".into(),
                    ));
                }
                tx.commit().map_err(|error| {
                    IronCrewError::Validation(format!(
                        "SQLite conversation idempotency commit error: {error}"
                    ))
                })?;
                Ok(ConversationIdempotencyCommit {
                    revision: next_revision,
                    replayable: response_body.is_some(),
                    already_completed: false,
                })
            })
            .await,
        )
    }

    async fn mark_idempotency_indeterminate(
        &self,
        key_hash: &str,
        attempt_id: &str,
        completed_at: &str,
        expires_at: &str,
    ) -> Result<bool> {
        validate_digest("idempotency key hash", key_hash)?;
        sqlite_parse_idempotency_time("idempotency completion time", completed_at)?;
        sqlite_parse_idempotency_time("idempotency retention expiry", expires_at)?;
        let conn = Arc::clone(&self.conn);
        let key_hash = key_hash.to_string();
        let attempt_id = attempt_id.to_string();
        let completed_at = completed_at.to_string();
        let expires_at = expires_at.to_string();
        flatten_join(
            tokio::task::spawn_blocking(move || {
                let mut conn = conn.lock().map_err(|error| {
                    IronCrewError::Validation(format!("SQLite lock error: {error}"))
                })?;
                let tx = conn
                    .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                    .map_err(|error| {
                        IronCrewError::Validation(format!(
                            "SQLite idempotency indeterminate transaction error: {error}"
                        ))
                    })?;
                let Some(mut record) = sqlite_idempotency_record(&tx, &key_hash)? else {
                    return Ok(false);
                };
                if record.attempt_id != attempt_id {
                    return Err(IronCrewError::Conflict(
                        "Idempotency attempt changed before indeterminate transition".into(),
                    ));
                }
                if record.state.is_terminal() {
                    return Ok(false);
                }
                sqlite_write_indeterminate(&tx, &mut record, &completed_at, &expires_at)?;
                tx.commit().map_err(|error| {
                    IronCrewError::Validation(format!(
                        "SQLite idempotency indeterminate commit error: {error}"
                    ))
                })?;
                Ok(true)
            })
            .await,
        )
    }

    async fn release_idempotency(&self, key_hash: &str, attempt_id: &str) -> Result<bool> {
        validate_digest("idempotency key hash", key_hash)?;
        let conn = Arc::clone(&self.conn);
        let key_hash = key_hash.to_string();
        let attempt_id = attempt_id.to_string();
        flatten_join(
            tokio::task::spawn_blocking(move || {
                let mut conn = conn.lock().map_err(|error| {
                    IronCrewError::Validation(format!("SQLite lock error: {error}"))
                })?;
                let tx = conn
                    .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                    .map_err(|error| {
                        IronCrewError::Validation(format!(
                            "SQLite idempotency release transaction error: {error}"
                        ))
                    })?;
                let Some(record) = sqlite_idempotency_record(&tx, &key_hash)? else {
                    return Ok(false);
                };
                if record.attempt_id != attempt_id {
                    return Err(IronCrewError::Conflict(
                        "Idempotency attempt changed before release".into(),
                    ));
                }
                if !record.state.is_in_flight() {
                    return Ok(false);
                }
                let changed = tx
                    .execute(
                        "DELETE FROM idempotency WHERE key_hash = ?1 AND attempt_id = ?2 \
                         AND state IN ('claimed', 'running')",
                        rusqlite::params![key_hash, attempt_id],
                    )
                    .map_err(|error| {
                        IronCrewError::Validation(format!(
                            "SQLite idempotency release error: {error}"
                        ))
                    })?;
                tx.commit().map_err(|error| {
                    IronCrewError::Validation(format!(
                        "SQLite idempotency release commit error: {error}"
                    ))
                })?;
                Ok(changed == 1)
            })
            .await,
        )
    }

    async fn prune_idempotency(&self, now: &str, limit: usize) -> Result<usize> {
        sqlite_parse_idempotency_time("idempotency prune time", now)?;
        let conn = Arc::clone(&self.conn);
        let now = now.to_string();
        flatten_join(
            tokio::task::spawn_blocking(move || {
                let mut conn = conn.lock().map_err(|error| {
                    IronCrewError::Validation(format!("SQLite lock error: {error}"))
                })?;
                let tx = conn
                    .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                    .map_err(|error| {
                        IronCrewError::Validation(format!(
                            "SQLite idempotency prune transaction error: {error}"
                        ))
                    })?;
                let removed = sqlite_prune_idempotency(&tx, &now, limit)?;
                tx.commit().map_err(|error| {
                    IronCrewError::Validation(format!(
                        "SQLite idempotency prune commit error: {error}"
                    ))
                })?;
                Ok(removed)
            })
            .await,
        )
    }

    async fn idempotency_usage(
        &self,
        principal_id: &PrincipalId,
        limits: IdempotencyLimits,
    ) -> Result<IdempotencyUsage> {
        limits.validate()?;
        let conn = Arc::clone(&self.conn);
        let principal_id = principal_id.clone();
        flatten_join(
            tokio::task::spawn_blocking(move || {
                let conn = conn.lock().map_err(|error| {
                    IronCrewError::Validation(format!("SQLite lock error: {error}"))
                })?;
                let mut statement = conn
                    .prepare(
                        "SELECT principal_id, COUNT(*), \
                                COALESCE(SUM(CASE WHEN state IN ('claimed', 'running') THEN 1 ELSE 0 END), 0), \
                                COALESCE(SUM(length(CAST(response_body AS BLOB))), 0) \
                         FROM idempotency GROUP BY principal_id",
                    )
                    .map_err(|error| {
                        IronCrewError::Validation(format!(
                            "SQLite idempotency usage prepare error: {error}"
                        ))
                    })?;
                let rows = statement
                    .query_map([], |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, i64>(1)?,
                            row.get::<_, i64>(2)?,
                            row.get::<_, i64>(3)?,
                        ))
                    })
                    .map_err(|error| {
                        IronCrewError::Validation(format!(
                            "SQLite idempotency usage query error: {error}"
                        ))
                    })?;
                let mut snapshot = IdempotencyUsage::default();
                for row in rows {
                    let (raw_principal, records, in_flight, response_bytes) = row.map_err(|error| {
                        IronCrewError::Validation(format!(
                            "SQLite idempotency usage row error: {error}"
                        ))
                    })?;
                    let id = PrincipalId::from_digest(raw_principal)?;
                    let records = usize::try_from(records).map_err(|_| {
                        IronCrewError::Validation(
                            "SQLite principal idempotency record count is out of range".into(),
                        )
                    })?;
                    let in_flight = usize::try_from(in_flight).map_err(|_| {
                        IronCrewError::Validation(
                            "SQLite principal idempotency in-flight count is out of range".into(),
                        )
                    })?;
                    let response_bytes = usize::try_from(response_bytes).map_err(|_| {
                        IronCrewError::Validation(
                            "SQLite principal idempotency response bytes are out of range".into(),
                        )
                    })?;
                    snapshot.principal_count = snapshot.principal_count.saturating_add(1);
                    snapshot.global_records = snapshot.global_records.saturating_add(records);
                    snapshot.global_in_flight = snapshot.global_in_flight.saturating_add(in_flight);
                    snapshot.global_response_bytes = snapshot
                        .global_response_bytes
                        .checked_add(response_bytes)
                        .ok_or_else(|| {
                            IronCrewError::Validation(
                                "SQLite idempotency response bytes overflow".into(),
                            )
                        })?;
                    snapshot.max_principal_records = snapshot.max_principal_records.max(records);
                    snapshot.max_principal_in_flight =
                        snapshot.max_principal_in_flight.max(in_flight);
                    snapshot.max_principal_response_bytes =
                        snapshot.max_principal_response_bytes.max(response_bytes);
                    if id == principal_id {
                        snapshot.principal_records = records;
                        snapshot.principal_in_flight = in_flight;
                        snapshot.principal_response_bytes = response_bytes;
                    }
                    let at = |percentage| {
                        sqlite_quota_at_or_above(
                            records,
                            limits.principal_max_records,
                            percentage,
                        ) || sqlite_quota_at_or_above(
                            in_flight,
                            limits.principal_max_in_flight,
                            percentage,
                        ) || sqlite_quota_at_or_above(
                            response_bytes,
                            limits.principal_max_response_bytes,
                            percentage,
                        )
                    };
                    snapshot.principals_at_or_above_80_percent += usize::from(at(80));
                    snapshot.principals_at_or_above_90_percent += usize::from(at(90));
                    snapshot.principals_at_or_above_100_percent += usize::from(at(100));
                }
                Ok(snapshot)
            })
            .await,
        )
    }

    async fn save_conversation(&self, record: &ConversationRecord) -> Result<u64> {
        let conn = Arc::clone(&self.conn);
        let record = record.clone();
        let next_revision = record
            .revision
            .checked_add(1)
            .ok_or_else(|| IronCrewError::Validation("Conversation revision overflow".into()))?;

        flatten_join(
            tokio::task::spawn_blocking(move || {
                validate_conversation_record_for_write(&record)?;
                let mut conn = conn
                    .lock()
                    .map_err(|e| IronCrewError::Validation(format!("SQLite lock error: {}", e)))?;
                let tx = conn
                    .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                    .map_err(|e| {
                        IronCrewError::Validation(format!(
                            "SQLite save_conversation transaction error: {e}"
                        ))
                    })?;

                if sqlite_active_conversation_idempotency(
                    &tx,
                    record.flow_path.as_deref(),
                    &record.id,
                    false,
                )? {
                    return Err(IronCrewError::Conflict(format!(
                        "Conversation '{}' has an active idempotent message operation",
                        record.id
                    )));
                }

                let messages_json = serialize_conversation_messages(&record.messages)?;
                let execution_json = serialize_conversation_execution(&record.execution)?;
                let current = match tx.query_row(
                    "SELECT revision, \
                            CASE WHEN length(CAST(execution AS BLOB)) <= ?3 THEN execution END, \
                            length(CAST(execution AS BLOB)) \
                     FROM conversations WHERE id = ?1 AND flow_path IS ?2",
                    rusqlite::params![
                        &record.id,
                        &record.flow_path,
                        i64::try_from(HARD_STORED_CONVERSATION_EXECUTION_BYTES)
                            .unwrap_or(i64::MAX),
                    ],
                    |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, Option<String>>(1)?,
                            row.get::<_, i64>(2)?,
                        ))
                    },
                ) {
                    Ok((revision, execution, execution_bytes)) => Some((
                        revision,
                        sqlite_bounded_conversation_execution(execution, execution_bytes, 1)?,
                    )),
                    Err(rusqlite::Error::QueryReturnedNoRows) => None,
                    Err(error) => {
                        return Err(IronCrewError::Validation(format!(
                            "SQLite save_conversation revision read error: {error}"
                        )));
                    }
                };
                let expected_revision = i64::try_from(record.revision).map_err(|_| {
                    IronCrewError::Validation("Conversation revision is out of range".into())
                })?;
                let next_revision_i64 = i64::try_from(next_revision).map_err(|_| {
                    IronCrewError::Validation("Conversation revision is out of range".into())
                })?;
                match current {
                    None if record.revision == 0 => {
                        tx.execute(
                            "INSERT INTO conversations \
                             (id, flow_name, flow_path, agent_name, execution, messages, created_at, updated_at, revision) \
                             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                            rusqlite::params![
                                &record.id,
                                &record.flow_name,
                                &record.flow_path,
                                &record.agent_name,
                                &execution_json,
                                &messages_json,
                                &record.created_at,
                                &record.updated_at,
                                next_revision_i64,
                            ],
                        )
                        .map_err(|e| {
                            IronCrewError::Validation(format!(
                                "SQLite save_conversation insert error: {e}"
                            ))
                        })?;
                    }
                    Some((current_revision, current_execution))
                        if current_revision == expected_revision
                            && current_execution == record.execution =>
                    {
                        let affected = tx
                            .execute(
                                "UPDATE conversations SET \
                                 flow_name = ?3, agent_name = ?4, execution = ?5, messages = ?6, \
                                 created_at = ?7, updated_at = ?8, revision = ?9 \
                                 WHERE id = ?1 AND flow_path IS ?2 AND revision = ?10",
                                rusqlite::params![
                                    &record.id,
                                    &record.flow_path,
                                    &record.flow_name,
                                    &record.agent_name,
                                    &execution_json,
                                    &messages_json,
                                    &record.created_at,
                                    &record.updated_at,
                                    next_revision_i64,
                                    expected_revision,
                                ],
                            )
                            .map_err(|e| {
                                IronCrewError::Validation(format!(
                                    "SQLite save_conversation update error: {e}"
                                ))
                            })?;
                        if affected != 1 {
                            return Err(IronCrewError::Conflict(format!(
                                "Conversation '{}' changed since revision {}; reopen it before saving",
                                record.id, record.revision
                            )));
                        }
                    }
                    _ => {
                        return Err(IronCrewError::Conflict(format!(
                            "Conversation '{}' changed since revision {}; reopen it before saving",
                            record.id, record.revision
                        )));
                    }
                }
                tx.commit().map_err(|e| {
                    IronCrewError::Validation(format!(
                        "SQLite save_conversation commit error: {e}"
                    ))
                })?;
                Ok(next_revision)
            })
            .await,
        )
    }

    async fn get_conversation(
        &self,
        flow_path: Option<&str>,
        id: &str,
    ) -> Result<Option<ConversationRecord>> {
        validate_session_id(id)?;
        let conn = Arc::clone(&self.conn);
        let flow_path = flow_path.map(|s| s.to_string());
        let id = id.to_string();

        flatten_join(
            tokio::task::spawn_blocking(move || {
                let conn = conn
                    .lock()
                    .map_err(|e| IronCrewError::Validation(format!("SQLite lock error: {}", e)))?;

                // Flow-scoped lookup: when `flow_path` is Some, require an exact
                // match. The SQL guards prevent cross-flow reads on the same id.
                let mut stmt = conn
                    .prepare(
                        "SELECT id, \
                                CASE WHEN length(CAST(flow_name AS BLOB)) <= ?3 THEN flow_name END, \
                                length(CAST(flow_name AS BLOB)), \
                                CASE WHEN flow_path IS NULL OR length(CAST(flow_path AS BLOB)) <= ?3 THEN flow_path END, \
                                length(CAST(flow_path AS BLOB)), \
                                CASE WHEN length(CAST(agent_name AS BLOB)) <= ?3 THEN agent_name END, \
                                length(CAST(agent_name AS BLOB)), \
                                CASE WHEN length(CAST(execution AS BLOB)) <= ?4 THEN execution END, \
                                length(CAST(execution AS BLOB)), \
                                CASE \
                                  WHEN length(CAST(messages AS BLOB)) <= ?5 \
                                   AND CASE WHEN json_valid(messages) \
                                            THEN CASE WHEN json_type(messages) = 'array' \
                                                      THEN json_array_length(messages) <= ?6 \
                                                      ELSE 0 END \
                                            ELSE 0 END \
                                  THEN messages \
                                END, \
                                length(CAST(messages AS BLOB)), \
                                CASE WHEN json_valid(messages) \
                                     THEN CASE WHEN json_type(messages) = 'array' \
                                               THEN json_array_length(messages) END \
                                END, \
                                CASE WHEN length(CAST(created_at AS BLOB)) <= ?3 THEN created_at END, \
                                length(CAST(created_at AS BLOB)), \
                                CASE WHEN length(CAST(updated_at AS BLOB)) <= ?3 THEN updated_at END, \
                                length(CAST(updated_at AS BLOB)), revision \
                         FROM conversations \
                         WHERE id = ?1 AND (?2 IS NULL OR flow_path = ?2)",
                    )
                    .map_err(|e| IronCrewError::Validation(format!("SQLite prepare error: {}", e)))?;

                let row = stmt
                    .query_row(
                        rusqlite::params![
                            id,
                            flow_path,
                            i64::try_from(HARD_STORED_CONVERSATION_METADATA_BYTES)
                                .unwrap_or(i64::MAX),
                            i64::try_from(HARD_STORED_CONVERSATION_EXECUTION_BYTES)
                                .unwrap_or(i64::MAX),
                            i64::try_from(HARD_STORED_CONVERSATION_MESSAGES_BYTES)
                                .unwrap_or(i64::MAX),
                            i64::try_from(HARD_STORED_CONVERSATION_MESSAGES)
                                .unwrap_or(i64::MAX),
                        ],
                        |row| {
                            Ok(BoundedConversationRow {
                                id: row.get(0)?,
                                flow_name: row.get(1)?,
                                flow_name_bytes: row.get(2)?,
                                flow_path: row.get(3)?,
                                flow_path_bytes: row.get(4)?,
                                agent_name: row.get(5)?,
                                agent_name_bytes: row.get(6)?,
                                execution: row.get(7)?,
                                execution_bytes: row.get(8)?,
                                messages: row.get(9)?,
                                messages_bytes: row.get(10)?,
                                message_count: row.get(11)?,
                                created_at: row.get(12)?,
                                created_at_bytes: row.get(13)?,
                                updated_at: row.get(14)?,
                                updated_at_bytes: row.get(15)?,
                                revision: row.get(16)?,
                            })
                        },
                    )
                    .map(Some)
                    .or_else(|e| match e {
                        rusqlite::Error::QueryReturnedNoRows => Ok(None),
                        other => Err(IronCrewError::Validation(format!(
                            "SQLite get_conversation error: {}",
                            other
                        ))),
                    })?;
                let Some(row) = row else {
                    return Ok(None);
                };
                let execution_bytes = sqlite_stored_bytes(row.execution_bytes, "execution")?;
                let messages_bytes = sqlite_stored_bytes(row.messages_bytes, "messages")?;
                let message_count = row
                    .message_count
                    .map(|count| sqlite_stored_bytes(count, "message count"))
                    .transpose()?;
                validate_stored_conversation_envelope(
                    execution_bytes,
                    messages_bytes,
                    message_count,
                )?;
                let execution_json = row.execution.ok_or_else(|| {
                    IronCrewError::Validation(
                        "SQLite stored conversation execution identity could not be materialized safely"
                            .into(),
                    )
                })?;
                let messages_json = row.messages.ok_or_else(|| {
                    IronCrewError::Validation(
                        "SQLite stored conversation messages could not be materialized safely".into(),
                    )
                })?;
                preflight_conversation_execution_json(&execution_json)?;
                preflight_conversation_messages_json(&messages_json)?;
                let record = ConversationRecord {
                    id: row.id,
                    flow_name: sqlite_bounded_metadata(
                        row.flow_name,
                        row.flow_name_bytes,
                        "flow name",
                    )?,
                    flow_path: sqlite_bounded_optional_metadata(
                        row.flow_path,
                        row.flow_path_bytes,
                        "flow path",
                    )?,
                    agent_name: sqlite_bounded_metadata(
                        row.agent_name,
                        row.agent_name_bytes,
                        "agent name",
                    )?,
                    execution: decode_stored_json(&execution_json, 7).map_err(|error| {
                        IronCrewError::Validation(format!(
                            "SQLite stored conversation execution identity has an invalid shape: {error}"
                        ))
                    })?,
                    messages: decode_stored_json(&messages_json, 9).map_err(|error| {
                        IronCrewError::Validation(format!(
                            "SQLite stored conversation messages have an invalid shape: {error}"
                        ))
                    })?,
                    created_at: sqlite_bounded_metadata(
                        row.created_at,
                        row.created_at_bytes,
                        "created timestamp",
                    )?,
                    updated_at: sqlite_bounded_metadata(
                        row.updated_at,
                        row.updated_at_bytes,
                        "updated timestamp",
                    )?,
                    revision: u64::try_from(row.revision).map_err(|_| {
                        IronCrewError::Validation(
                            "SQLite stored conversation revision is negative".into(),
                        )
                    })?,
                };
                validate_conversation_record_after_decode(&record)?;
                Ok(Some(record))
            })
            .await,
        )
    }

    async fn delete_conversation(&self, flow_path: Option<&str>, id: &str) -> Result<()> {
        validate_session_id(id)?;
        let conn = Arc::clone(&self.conn);
        let flow_path = flow_path.map(|s| s.to_string());
        let id = id.to_string();

        flatten_join(
            tokio::task::spawn_blocking(move || {
                let mut conn = conn
                    .lock()
                    .map_err(|e| IronCrewError::Validation(format!("SQLite lock error: {}", e)))?;
                let tx = conn
                    .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                    .map_err(|error| {
                        IronCrewError::Validation(format!(
                            "SQLite delete_conversation transaction error: {error}"
                        ))
                    })?;
                if sqlite_active_conversation_idempotency(&tx, flow_path.as_deref(), &id, true)? {
                    return Err(IronCrewError::Conflict(format!(
                        "Conversation '{id}' has an active idempotent message operation"
                    )));
                }
                tx.execute(
                    "DELETE FROM conversations WHERE id = ?1 AND (?2 IS NULL OR flow_path = ?2)",
                    rusqlite::params![&id, &flow_path],
                )
                .map_err(|e| {
                    IronCrewError::Validation(format!("SQLite delete_conversation error: {}", e))
                })?;
                tx.commit().map_err(|error| {
                    IronCrewError::Validation(format!(
                        "SQLite delete_conversation commit error: {error}"
                    ))
                })?;
                Ok(())
            })
            .await,
        )
    }

    async fn list_conversations(
        &self,
        flow_path: Option<&str>,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<ConversationSummary>> {
        let conn = Arc::clone(&self.conn);
        let flow_path = flow_path.map(|s| s.to_string());

        flatten_join(
            tokio::task::spawn_blocking(move || {
                let conn = conn
                    .lock()
                    .map_err(|e| IronCrewError::Validation(format!("SQLite lock error: {}", e)))?;
                let mut stmt = conn
                    .prepare(
                        "SELECT \
                           CASE WHEN length(CAST(c.id AS BLOB)) <= ?4 THEN c.id END, \
                           length(CAST(c.id AS BLOB)), \
                           CASE WHEN c.flow_path IS NULL OR length(CAST(c.flow_path AS BLOB)) <= ?4 \
                                THEN c.flow_path END, \
                           length(CAST(c.flow_path AS BLOB)), \
                           CASE WHEN length(CAST(c.agent_name AS BLOB)) <= ?4 THEN c.agent_name END, \
                           length(CAST(c.agent_name AS BLOB)), \
                           (SELECT COUNT(*) FROM json_each( \
                              CASE WHEN length(CAST(c.messages AS BLOB)) <= ?5 \
                                THEN CASE WHEN json_valid(c.messages) \
                                  THEN CASE WHEN json_type(c.messages) = 'array' \
                                    THEN CASE WHEN json_array_length(c.messages) <= ?6 \
                                      THEN c.messages ELSE '[]' END \
                                    ELSE '[]' END \
                                  ELSE '[]' END \
                                ELSE '[]' END \
                            ) AS message \
                            WHERE CASE WHEN message.type = 'object' \
                                       THEN json_extract(message.value, '$.role') END = 'user'), \
                           length(CAST(c.messages AS BLOB)), \
                           CASE WHEN json_valid(c.messages) \
                                THEN CASE WHEN json_type(c.messages) = 'array' \
                                          THEN json_array_length(c.messages) END END, \
                           CASE WHEN length(CAST(c.created_at AS BLOB)) <= ?4 THEN c.created_at END, \
                           length(CAST(c.created_at AS BLOB)), \
                           CASE WHEN length(CAST(c.updated_at AS BLOB)) <= ?4 THEN c.updated_at END \
                                AS bounded_updated_at, \
                           length(CAST(c.updated_at AS BLOB)) \
                         FROM conversations AS c \
                         WHERE (?1 IS NULL OR c.flow_path = ?1) \
                         ORDER BY bounded_updated_at DESC \
                         LIMIT ?2 OFFSET ?3",
                    )
                    .map_err(|e| {
                    IronCrewError::Validation(format!("SQLite prepare error: {}", e))
                })?;
                let limit = if limit == 0 {
                    i64::MAX
                } else {
                    i64::try_from(limit).unwrap_or(i64::MAX)
                };
                let rows = stmt
                    .query_map(
                        rusqlite::params![
                            flow_path,
                            limit,
                            i64::try_from(offset).unwrap_or(i64::MAX),
                            i64::try_from(HARD_STORED_CONVERSATION_METADATA_BYTES)
                                .unwrap_or(i64::MAX),
                            i64::try_from(HARD_STORED_CONVERSATION_MESSAGES_BYTES)
                                .unwrap_or(i64::MAX),
                            i64::try_from(HARD_STORED_CONVERSATION_MESSAGES)
                                .unwrap_or(i64::MAX),
                        ],
                        |row| {
                            Ok(BoundedConversationSummaryRow {
                                id: row.get(0)?,
                                id_bytes: row.get(1)?,
                                flow_path: row.get(2)?,
                                flow_path_bytes: row.get(3)?,
                                agent_name: row.get(4)?,
                                agent_name_bytes: row.get(5)?,
                                turn_count: row.get(6)?,
                                messages_bytes: row.get(7)?,
                                message_count: row.get(8)?,
                                created_at: row.get(9)?,
                                created_at_bytes: row.get(10)?,
                                updated_at: row.get(11)?,
                                updated_at_bytes: row.get(12)?,
                            })
                        },
                    )
                    .map_err(|_| {
                        IronCrewError::Validation(
                            "SQLite stored conversation summary is corrupt or exceeds hard limits"
                                .into(),
                        )
                    })?;

                let mut summaries = Vec::new();
                for row in rows {
                    let row = row.map_err(|_| {
                        IronCrewError::Validation(
                            "SQLite stored conversation summary is corrupt or exceeds hard limits"
                                .into(),
                        )
                    })?;
                    summaries.push(sqlite_conversation_summary(row).map_err(|_| {
                        IronCrewError::Validation(
                            "SQLite stored conversation summary is corrupt or exceeds hard limits"
                                .into(),
                        )
                    })?);
                }
                Ok(summaries)
            })
            .await,
        )
    }

    async fn count_conversations(&self, flow_path: Option<&str>) -> Result<u64> {
        let conn = Arc::clone(&self.conn);
        let flow_path = flow_path.map(|s| s.to_string());

        flatten_join(
            tokio::task::spawn_blocking(move || {
                let conn = conn
                    .lock()
                    .map_err(|e| IronCrewError::Validation(format!("SQLite lock error: {}", e)))?;

                let (sql, param): (String, Option<String>) = match flow_path {
                    Some(fp) => (
                        "SELECT COUNT(*) FROM conversations WHERE flow_path = ?1".into(),
                        Some(fp),
                    ),
                    None => ("SELECT COUNT(*) FROM conversations".into(), None),
                };

                let count: i64 = match param {
                    Some(fp) => conn
                        .query_row(&sql, rusqlite::params![fp], |row| row.get(0))
                        .map_err(|e| {
                            IronCrewError::Validation(format!("SQLite count error: {}", e))
                        })?,
                    None => conn.query_row(&sql, [], |row| row.get(0)).map_err(|e| {
                        IronCrewError::Validation(format!("SQLite count error: {}", e))
                    })?,
                };
                Ok(count as u64)
            })
            .await,
        )
    }

    async fn save_dialog_state(&self, record: &DialogStateRecord) -> Result<u64> {
        let conn = Arc::clone(&self.conn);
        let record = record.clone();
        let next_revision = record
            .revision
            .checked_add(1)
            .ok_or_else(|| IronCrewError::Validation("Dialog revision overflow".into()))?;

        flatten_join(
            tokio::task::spawn_blocking(move || {
                let mut conn = conn
                    .lock()
                    .map_err(|e| IronCrewError::Validation(format!("SQLite lock error: {}", e)))?;
                let tx = conn
                    .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                    .map_err(|e| {
                        IronCrewError::Validation(format!(
                            "SQLite save_dialog_state transaction error: {e}"
                        ))
                    })?;

                let agents_json = serde_json::to_string(&record.agent_names).map_err(|e| {
                    IronCrewError::Validation(format!("Failed to serialize agent_names: {}", e))
                })?;
                let transcript_json = serde_json::to_string(&record.transcript).map_err(|e| {
                    IronCrewError::Validation(format!("Failed to serialize transcript: {}", e))
                })?;

                let current_revision = match tx.query_row(
                    "SELECT revision FROM dialogs WHERE id = ?1 AND flow_path IS ?2",
                    rusqlite::params![&record.id, &record.flow_path],
                    |row| row.get::<_, i64>(0),
                ) {
                    Ok(value) => Some(value),
                    Err(rusqlite::Error::QueryReturnedNoRows) => None,
                    Err(error) => {
                        return Err(IronCrewError::Validation(format!(
                            "SQLite save_dialog_state revision read error: {error}"
                        )));
                    }
                };
                let expected_revision = i64::try_from(record.revision).map_err(|_| {
                    IronCrewError::Validation("Dialog revision is out of range".into())
                })?;
                let next_revision_i64 = i64::try_from(next_revision).map_err(|_| {
                    IronCrewError::Validation("Dialog revision is out of range".into())
                })?;
                match current_revision {
                    None if record.revision == 0 => {
                        tx.execute(
                            "INSERT INTO dialogs \
                             (id, flow_name, flow_path, agent_names, starter, transcript, next_index, stopped, stop_reason, created_at, updated_at, revision) \
                             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                            rusqlite::params![
                                &record.id,
                                &record.flow_name,
                                &record.flow_path,
                                &agents_json,
                                &record.starter,
                                &transcript_json,
                                record.next_index as i64,
                                record.stopped as i64,
                                &record.stop_reason,
                                &record.created_at,
                                &record.updated_at,
                                next_revision_i64,
                            ],
                        )
                        .map_err(|e| {
                            IronCrewError::Validation(format!(
                                "SQLite save_dialog_state insert error: {e}"
                            ))
                        })?;
                    }
                    Some(current) if current == expected_revision => {
                        let affected = tx
                            .execute(
                                "UPDATE dialogs SET \
                                 flow_name = ?3, agent_names = ?4, starter = ?5, \
                                 transcript = ?6, next_index = ?7, stopped = ?8, \
                                 stop_reason = ?9, created_at = ?10, updated_at = ?11, \
                                 revision = ?12 \
                                 WHERE id = ?1 AND flow_path IS ?2 AND revision = ?13",
                                rusqlite::params![
                                    &record.id,
                                    &record.flow_path,
                                    &record.flow_name,
                                    &agents_json,
                                    &record.starter,
                                    &transcript_json,
                                    record.next_index as i64,
                                    record.stopped as i64,
                                    &record.stop_reason,
                                    &record.created_at,
                                    &record.updated_at,
                                    next_revision_i64,
                                    expected_revision,
                                ],
                            )
                            .map_err(|e| {
                                IronCrewError::Validation(format!(
                                    "SQLite save_dialog_state update error: {e}"
                                ))
                            })?;
                        if affected != 1 {
                            return Err(IronCrewError::Conflict(format!(
                                "Dialog '{}' changed since revision {}; reopen it before saving",
                                record.id, record.revision
                            )));
                        }
                    }
                    _ => {
                        return Err(IronCrewError::Conflict(format!(
                            "Dialog '{}' changed since revision {}; reopen it before saving",
                            record.id, record.revision
                        )));
                    }
                }
                tx.commit().map_err(|e| {
                    IronCrewError::Validation(format!(
                        "SQLite save_dialog_state commit error: {e}"
                    ))
                })?;
                Ok(next_revision)
            })
            .await,
        )
    }

    async fn get_dialog_state(
        &self,
        flow_path: Option<&str>,
        id: &str,
    ) -> Result<Option<DialogStateRecord>> {
        let conn = Arc::clone(&self.conn);
        let flow_path = flow_path.map(|s| s.to_string());
        let id = id.to_string();

        flatten_join(
            tokio::task::spawn_blocking(move || {
                let conn = conn
                    .lock()
                    .map_err(|e| IronCrewError::Validation(format!("SQLite lock error: {}", e)))?;

                let mut stmt = conn
                    .prepare(
                        "SELECT id, flow_name, flow_path, agent_names, starter, transcript, next_index, \
                         stopped, stop_reason, created_at, updated_at, revision \
                         FROM dialogs \
                         WHERE id = ?1 AND (?2 IS NULL OR flow_path = ?2)",
                    )
                    .map_err(|e| IronCrewError::Validation(format!("SQLite prepare error: {}", e)))?;

                let row = stmt
                    .query_row(rusqlite::params![id, flow_path], |row| {
                        let agents_json: String = row.get(3)?;
                        let transcript_json: String = row.get(5)?;
                        Ok(DialogStateRecord {
                            id: row.get(0)?,
                            flow_name: row.get(1)?,
                            flow_path: row.get(2)?,
                            agent_names: decode_stored_json(&agents_json, 3)?,
                            starter: row.get(4)?,
                            transcript: decode_stored_json(&transcript_json, 5)?,
                            next_index: row.get::<_, i64>(6)? as usize,
                            stopped: row.get::<_, i64>(7)? != 0,
                            stop_reason: row.get(8)?,
                            created_at: row.get(9)?,
                            updated_at: row.get(10)?,
                            revision: u64::try_from(row.get::<_, i64>(11)?).map_err(|_| {
                                rusqlite::Error::IntegralValueOutOfRange(11, i64::MIN)
                            })?,
                        })
                    })
                    .map(Some)
                    .or_else(|e| match e {
                        rusqlite::Error::QueryReturnedNoRows => Ok(None),
                        other => Err(IronCrewError::Validation(format!(
                            "SQLite get_dialog_state error: {}",
                            other
                        ))),
                    })?;
                Ok(row)
            })
            .await,
        )
    }

    async fn delete_dialog_state(&self, flow_path: Option<&str>, id: &str) -> Result<()> {
        let conn = Arc::clone(&self.conn);
        let flow_path = flow_path.map(|s| s.to_string());
        let id = id.to_string();

        flatten_join(
            tokio::task::spawn_blocking(move || {
                let conn = conn
                    .lock()
                    .map_err(|e| IronCrewError::Validation(format!("SQLite lock error: {}", e)))?;
                conn.execute(
                    "DELETE FROM dialogs WHERE id = ?1 AND (?2 IS NULL OR flow_path = ?2)",
                    rusqlite::params![id, flow_path],
                )
                .map_err(|e| {
                    IronCrewError::Validation(format!("SQLite delete_dialog_state error: {}", e))
                })?;
                Ok(())
            })
            .await,
        )
    }

    async fn save_audit_event(&self, event: &crate::engine::audit::AuditEvent) -> Result<String> {
        let conn = Arc::clone(&self.conn);
        let event = event.clone();

        flatten_join(
            tokio::task::spawn_blocking(move || {
                let conn = conn
                    .lock()
                    .map_err(|e| IronCrewError::Validation(format!("SQLite lock error: {}", e)))?;

                let id = uuid::Uuid::new_v4().to_string();
                let metadata_json = match &event.metadata {
                    Some(v) => Some(serde_json::to_string(v).map_err(|e| {
                        IronCrewError::Validation(format!("Failed to serialize metadata: {}", e))
                    })?),
                    None => None,
                };

                conn.execute(
                    "INSERT INTO audit_events (id, timestamp, action, flow_path, target, actor, source_ip, success, status_code, metadata)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                    rusqlite::params![
                        id,
                        event.timestamp,
                        event.action,
                        event.flow_path,
                        event.target,
                        event.actor,
                        event.source_ip,
                        if event.success { 1 } else { 0 },
                        event.status_code as i64,
                        metadata_json,
                    ],
                )
                .map_err(|e| {
                    IronCrewError::Validation(format!("SQLite insert audit event: {}", e))
                })?;
                tracing::debug!("Audit event saved: {}", id);
                Ok(id)
            })
            .await,
        )
    }

    async fn list_audit_events(
        &self,
        filter: &crate::engine::audit::AuditFilter,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<crate::engine::audit::AuditEvent>> {
        let conn = Arc::clone(&self.conn);
        let wc = store_sql::audit_where(filter, Dialect::Sqlite);

        flatten_join(
            tokio::task::spawn_blocking(move || {
                let conn = conn
                    .lock()
                    .map_err(|e| IronCrewError::Validation(format!("SQLite lock error: {}", e)))?;

                let mut sql = format!(
                    "SELECT id, timestamp, action, flow_path, target, actor, source_ip, success, status_code, metadata \
                     FROM audit_events{}",
                    wc.sql
                );
                sql.push_str(" ORDER BY timestamp DESC");
                if limit > 0 {
                    sql.push_str(&format!(" LIMIT {}", limit));
                }
                if offset > 0 {
                    sql.push_str(&format!(" OFFSET {}", offset));
                }

                let boxed = to_sql_params(wc.params);
                let refs: Vec<&dyn rusqlite::types::ToSql> = boxed.iter().map(|b| b.as_ref()).collect();

                let mut stmt = conn
                    .prepare(&sql)
                    .map_err(|e| IronCrewError::Validation(format!("SQLite prepare: {}", e)))?;
                let rows = stmt
                    .query_map(rusqlite::params_from_iter(refs), |row| {
                        let metadata_json: Option<String> = row.get(9)?;
                        Ok(crate::engine::audit::AuditEvent {
                            id: row.get(0)?,
                            timestamp: row.get(1)?,
                            action: row.get(2)?,
                            flow_path: row.get(3)?,
                            target: row.get(4)?,
                            actor: row.get(5)?,
                            source_ip: row.get(6)?,
                            success: row.get::<_, i64>(7)? != 0,
                            status_code: row.get::<_, i64>(8)? as u16,
                            metadata: metadata_json
                                .as_deref()
                                .map(|raw| decode_stored_json(raw, 9))
                                .transpose()?,
                        })
                    })
                    .map_err(|e| IronCrewError::Validation(format!("SQLite query: {}", e)))?;

                let mut events = Vec::new();
                for r in rows {
                    events.push(
                        r.map_err(|e| IronCrewError::Validation(format!("SQLite row: {}", e)))?,
                    );
                }
                Ok(events)
            })
            .await,
        )
    }

    async fn count_audit_events(&self, filter: &crate::engine::audit::AuditFilter) -> Result<u64> {
        let conn = Arc::clone(&self.conn);
        let wc = store_sql::audit_where(filter, Dialect::Sqlite);

        flatten_join(
            tokio::task::spawn_blocking(move || {
                let conn = conn
                    .lock()
                    .map_err(|e| IronCrewError::Validation(format!("SQLite lock error: {}", e)))?;

                let sql = format!("SELECT COUNT(*) FROM audit_events{}", wc.sql);
                let boxed = to_sql_params(wc.params);
                let refs: Vec<&dyn rusqlite::types::ToSql> =
                    boxed.iter().map(|b| b.as_ref()).collect();

                let count: i64 = conn
                    .query_row(&sql, rusqlite::params_from_iter(refs), |row| row.get(0))
                    .map_err(|e| IronCrewError::Validation(format!("SQLite count: {}", e)))?;
                Ok(count as u64)
            })
            .await,
        )
    }
}
