use async_trait::async_trait;
use rusqlite::Connection;
use serde::de::DeserializeOwned;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use super::run_history::{
    ListRunsFilter, RunCompletion, RunIntent, RunRecord, RunStatus, RunSummary, RunTransition,
};
use super::sessions::{ConversationRecord, ConversationSummary, DialogStateRecord};
use super::store::{RunLeaseConfig, StateStore};
use super::store_sql::{self, Dialect, SqlParam};
use crate::utils::error::{IronCrewError, Result};

pub struct SqliteStore {
    conn: Arc<Mutex<Connection>>,
    lease: RunLeaseConfig,
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
                ON audit_events (flow_path);",
        )
        .map_err(|e| IronCrewError::Validation(format!("Failed to create SQLite tables: {}", e)))?;

        // Idempotent ALTER TABLE migrations for schemas predating the
        // `flow_path` column. SQLite's ADD COLUMN is atomic and safe to
        // retry; errors ("duplicate column name") are swallowed.
        for sql in [
            "ALTER TABLE conversations ADD COLUMN flow_path TEXT",
            "ALTER TABLE dialogs ADD COLUMN flow_path TEXT",
            "ALTER TABLE conversations ADD COLUMN revision INTEGER NOT NULL DEFAULT 0",
            "ALTER TABLE dialogs ADD COLUMN revision INTEGER NOT NULL DEFAULT 0",
        ] {
            if let Err(e) = conn.execute(sql, []) {
                let msg = e.to_string();
                if !msg.contains("duplicate column") {
                    tracing::debug!("SQLite ADD COLUMN skipped: {}", msg);
                }
            }
        }

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
             messages    TEXT NOT NULL,
             created_at  TEXT NOT NULL,
             updated_at  TEXT NOT NULL,
             revision    INTEGER NOT NULL DEFAULT 0,
             UNIQUE (flow_path, id)
         );
         INSERT OR IGNORE INTO conversations_new
             (id, flow_name, flow_path, agent_name, messages, created_at, updated_at, revision)
             SELECT id, flow_name, flow_path, agent_name, messages, created_at, updated_at, revision
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

#[async_trait]
impl StateStore for SqliteStore {
    async fn save_run_intent(&self, intent: RunIntent) -> Result<String> {
        let conn = Arc::clone(&self.conn);
        let owner_instance_id = self.lease.instance_id().to_string();
        let lease_expires_at = self.lease.deadline_now();

        flatten_join(
            tokio::task::spawn_blocking(move || {
                let conn = conn
                    .lock()
                    .map_err(|e| IronCrewError::Validation(format!("SQLite lock error: {}", e)))?;
                let run_id = intent
                    .suggested_id
                    .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
                let tags_json = serde_json::to_string(&intent.tags).map_err(|e| {
                    IronCrewError::Validation(format!("Failed to serialize tags: {}", e))
                })?;

                conn.execute(
                    "INSERT INTO runs (run_id, flow_name, flow, status, started_at, finished_at, duration_ms, task_results, agent_count, task_count, total_tokens, cached_tokens, tags, owner_instance_id, lease_expires_at)
                     VALUES (?1, ?2, ?3, 'running', ?4, '', 0, '[]', ?5, ?6, 0, 0, ?7, ?8, ?9)",
                    rusqlite::params![
                        run_id,
                        intent.flow_name,
                        intent.flow,
                        intent.started_at,
                        intent.agent_count as i64,
                        intent.task_count as i64,
                        tags_json,
                        owner_instance_id,
                        lease_expires_at,
                    ],
                )
                .map_err(|e| IronCrewError::Validation(format!("SQLite insert intent: {}", e)))?;
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
                let conn = conn
                    .lock()
                    .map_err(|e| IronCrewError::Validation(format!("SQLite lock error: {}", e)))?;
                let task_results_json =
                    serde_json::to_string(&completion.task_results).map_err(|e| {
                        IronCrewError::Validation(format!(
                            "Failed to serialize task_results: {}",
                            e
                        ))
                    })?;

                let rows = conn
                    .execute(
                        "UPDATE runs
                         SET status = ?1, finished_at = ?2, duration_ms = ?3,
                             task_results = ?4, total_tokens = ?5, cached_tokens = ?6,
                             lease_expires_at = ''
                         WHERE run_id = ?7 AND status IN ('running', 'waiting_for_input')
                           AND owner_instance_id = ?8",
                        rusqlite::params![
                            completion.status.to_string(),
                            completion.finished_at,
                            completion.duration_ms as i64,
                            task_results_json,
                            completion.total_tokens as i64,
                            completion.cached_tokens as i64,
                            run_id,
                            owner_instance_id,
                        ],
                    )
                    .map_err(|e| {
                        IronCrewError::Validation(format!("SQLite update completion: {}", e))
                    })?;

                if rows == 0 {
                    let state = conn.query_row(
                        "SELECT status, owner_instance_id FROM runs WHERE run_id = ?1",
                        rusqlite::params![run_id],
                        |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                    );
                    return match state {
                        Ok((status, _)) if status.parse::<RunStatus>()?.is_terminal() => {
                            Ok(RunTransition::AlreadyTerminal(status.parse()?))
                        }
                        Ok((_, owner)) => Err(IronCrewError::Validation(format!(
                            "Run '{}' is owned by instance '{}', not '{}'",
                            run_id, owner, owner_instance_id
                        ))),
                        Err(rusqlite::Error::QueryReturnedNoRows) => Err(
                            IronCrewError::Validation(format!("Run '{}' not found", run_id)),
                        ),
                        Err(e) => Err(IronCrewError::Validation(format!(
                            "SQLite completion state query: {}",
                            e
                        ))),
                    };
                }
                tracing::info!("Run completion saved: {} ({})", run_id, completion.status);
                Ok(RunTransition::Applied)
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
                let conn = conn
                    .lock()
                    .map_err(|e| IronCrewError::Validation(format!("SQLite lock error: {}", e)))?;
                conn.execute(
                    "UPDATE runs SET lease_expires_at = ?1
                     WHERE owner_instance_id = ?2
                       AND status IN ('running', 'waiting_for_input')",
                    rusqlite::params![lease_expires_at, owner_instance_id],
                )
                .map_err(|e| IronCrewError::Validation(format!("SQLite heartbeat: {}", e)))
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
                let conn = conn
                    .lock()
                    .map_err(|e| IronCrewError::Validation(format!("SQLite lock error: {}", e)))?;
                let rows = conn
                    .execute(
                        "UPDATE runs
                         SET status = 'abandoned', finished_at = ?1, lease_expires_at = ''
                         WHERE status IN ('running', 'waiting_for_input')
                           AND (lease_expires_at = '' OR lease_expires_at <= ?2)",
                        rusqlite::params![now, normalized_now],
                    )
                    .map_err(|e| IronCrewError::Validation(format!("SQLite reconcile: {}", e)))?;
                Ok(rows)
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

    async fn save_conversation(&self, record: &ConversationRecord) -> Result<u64> {
        let conn = Arc::clone(&self.conn);
        let record = record.clone();
        let next_revision = record
            .revision
            .checked_add(1)
            .ok_or_else(|| IronCrewError::Validation("Conversation revision overflow".into()))?;

        flatten_join(
            tokio::task::spawn_blocking(move || {
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

                let messages_json = serde_json::to_string(&record.messages).map_err(|e| {
                    IronCrewError::Validation(format!("Failed to serialize messages: {}", e))
                })?;
                let current_revision = match tx.query_row(
                    "SELECT revision FROM conversations WHERE id = ?1 AND flow_path IS ?2",
                    rusqlite::params![&record.id, &record.flow_path],
                    |row| row.get::<_, i64>(0),
                ) {
                    Ok(value) => Some(value),
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
                match current_revision {
                    None if record.revision == 0 => {
                        tx.execute(
                            "INSERT INTO conversations \
                             (id, flow_name, flow_path, agent_name, messages, created_at, updated_at, revision) \
                             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                            rusqlite::params![
                                &record.id,
                                &record.flow_name,
                                &record.flow_path,
                                &record.agent_name,
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
                    Some(current) if current == expected_revision => {
                        let affected = tx
                            .execute(
                                "UPDATE conversations SET \
                                 flow_name = ?3, agent_name = ?4, messages = ?5, \
                                 created_at = ?6, updated_at = ?7, revision = ?8 \
                                 WHERE id = ?1 AND flow_path IS ?2 AND revision = ?9",
                                rusqlite::params![
                                    &record.id,
                                    &record.flow_path,
                                    &record.flow_name,
                                    &record.agent_name,
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
                        "SELECT id, flow_name, flow_path, agent_name, messages, created_at, updated_at, revision \
                         FROM conversations \
                         WHERE id = ?1 AND (?2 IS NULL OR flow_path = ?2)",
                    )
                    .map_err(|e| IronCrewError::Validation(format!("SQLite prepare error: {}", e)))?;

                let row = stmt
                    .query_row(rusqlite::params![id, flow_path], |row| {
                        let messages_json: String = row.get(4)?;
                        Ok(ConversationRecord {
                            id: row.get(0)?,
                            flow_name: row.get(1)?,
                            flow_path: row.get(2)?,
                            agent_name: row.get(3)?,
                            messages: decode_stored_json(&messages_json, 4)?,
                            created_at: row.get(5)?,
                            updated_at: row.get(6)?,
                            revision: u64::try_from(row.get::<_, i64>(7)?).map_err(|_| {
                                rusqlite::Error::IntegralValueOutOfRange(7, i64::MIN)
                            })?,
                        })
                    })
                    .map(Some)
                    .or_else(|e| match e {
                        rusqlite::Error::QueryReturnedNoRows => Ok(None),
                        other => Err(IronCrewError::Validation(format!(
                            "SQLite get_conversation error: {}",
                            other
                        ))),
                    })?;
                Ok(row)
            })
            .await,
        )
    }

    async fn delete_conversation(&self, flow_path: Option<&str>, id: &str) -> Result<()> {
        let conn = Arc::clone(&self.conn);
        let flow_path = flow_path.map(|s| s.to_string());
        let id = id.to_string();

        flatten_join(
            tokio::task::spawn_blocking(move || {
                let conn = conn
                    .lock()
                    .map_err(|e| IronCrewError::Validation(format!("SQLite lock error: {}", e)))?;
                conn.execute(
                    "DELETE FROM conversations WHERE id = ?1 AND (?2 IS NULL OR flow_path = ?2)",
                    rusqlite::params![id, flow_path],
                )
                .map_err(|e| {
                    IronCrewError::Validation(format!("SQLite delete_conversation error: {}", e))
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

                let mut sql = String::from(
                    "SELECT c.id, c.flow_path, c.agent_name, \
                            (SELECT COUNT(*) FROM json_each(c.messages) AS message \
                             WHERE json_extract(message.value, '$.role') = 'user') AS turn_count, \
                            c.created_at, c.updated_at \
                     FROM conversations AS c",
                );
                let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
                let mut next_idx = 1usize;

                if let Some(fp) = flow_path {
                    sql.push_str(&format!(" WHERE flow_path = ?{}", next_idx));
                    params.push(Box::new(fp));
                    next_idx += 1;
                }
                sql.push_str(" ORDER BY updated_at DESC");
                if limit > 0 {
                    sql.push_str(&format!(" LIMIT ?{}", next_idx));
                    params.push(Box::new(limit as i64));
                    next_idx += 1;
                    if offset > 0 {
                        sql.push_str(&format!(" OFFSET ?{}", next_idx));
                        params.push(Box::new(offset as i64));
                    }
                }

                let mut stmt = conn.prepare(&sql).map_err(|e| {
                    IronCrewError::Validation(format!("SQLite prepare error: {}", e))
                })?;
                let param_refs: Vec<&dyn rusqlite::types::ToSql> =
                    params.iter().map(|p| p.as_ref()).collect();

                let rows = stmt
                    .query_map(param_refs.as_slice(), |row| {
                        Ok(ConversationSummary {
                            id: row.get(0)?,
                            flow_path: row.get(1)?,
                            agent_name: row.get(2)?,
                            created_at: row.get(4)?,
                            updated_at: row.get(5)?,
                            turn_count: row.get::<_, i64>(3)? as usize,
                        })
                    })
                    .map_err(|e| IronCrewError::Validation(format!("SQLite query error: {}", e)))?;

                let mut summaries = Vec::new();
                for s in rows.flatten() {
                    summaries.push(s);
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
