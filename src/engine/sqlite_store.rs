use async_trait::async_trait;
use rusqlite::Connection;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use super::run_history::{
    ListRunsFilter, RunCompletion, RunIntent, RunRecord, RunStatus, RunSummary,
};
use super::sessions::{ConversationRecord, ConversationSummary, DialogStateRecord};
use super::store::StateStore;
use super::store_sql::{self, Dialect, SqlParam};
use crate::utils::error::{IronCrewError, Result};

pub struct SqliteStore {
    conn: Arc<Mutex<Connection>>,
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

impl SqliteStore {
    pub fn new(db_path: PathBuf) -> Result<Self> {
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
                created_at TEXT DEFAULT (datetime('now'))
            );

            CREATE TABLE IF NOT EXISTS conversations (
                id TEXT PRIMARY KEY,
                flow_name TEXT NOT NULL,
                agent_name TEXT NOT NULL,
                messages TEXT NOT NULL,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
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
                updated_at TEXT NOT NULL
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
             UNIQUE (flow_path, id)
         );
         INSERT OR IGNORE INTO conversations_new
             (id, flow_name, flow_path, agent_name, messages, created_at, updated_at)
             SELECT id, flow_name, flow_path, agent_name, messages, created_at, updated_at
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
             UNIQUE (flow_path, id)
         );
         INSERT OR IGNORE INTO dialogs_new
             (id, flow_name, flow_path, agent_names, starter, transcript,
              next_index, stopped, stop_reason, created_at, updated_at)
             SELECT id, flow_name, flow_path, agent_names, starter, transcript,
                    next_index, stopped, stop_reason, created_at, updated_at
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
                    "INSERT INTO runs (run_id, flow_name, flow, status, started_at, finished_at, duration_ms, task_results, agent_count, task_count, total_tokens, cached_tokens, tags)
                     VALUES (?1, ?2, ?3, 'running', ?4, '', 0, '[]', ?5, ?6, 0, 0, ?7)",
                    rusqlite::params![
                        run_id,
                        intent.flow_name,
                        intent.flow,
                        intent.started_at,
                        intent.agent_count as i64,
                        intent.task_count as i64,
                        tags_json,
                    ],
                )
                .map_err(|e| IronCrewError::Validation(format!("SQLite insert intent: {}", e)))?;
                tracing::debug!("Run intent saved: {}", run_id);
                Ok(run_id)
            })
            .await,
        )
    }

    async fn update_run_completion(&self, run_id: &str, completion: RunCompletion) -> Result<()> {
        let conn = Arc::clone(&self.conn);
        let run_id = run_id.to_string();

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
                             task_results = ?4, total_tokens = ?5, cached_tokens = ?6
                         WHERE run_id = ?7 AND status IN ('running', 'waiting_for_input')",
                        rusqlite::params![
                            completion.status.to_string(),
                            completion.finished_at,
                            completion.duration_ms as i64,
                            task_results_json,
                            completion.total_tokens as i64,
                            completion.cached_tokens as i64,
                            run_id,
                        ],
                    )
                    .map_err(|e| {
                        IronCrewError::Validation(format!("SQLite update completion: {}", e))
                    })?;

                if rows == 0 {
                    return Err(IronCrewError::Validation(format!(
                        "Run '{}' not found or not in an in-flight state",
                        run_id
                    )));
                }
                tracing::info!("Run completion saved: {} ({})", run_id, completion.status);
                Ok(())
            })
            .await,
        )
    }

    async fn update_run_status(
        &self,
        run_id: &str,
        status: crate::engine::run_history::RunStatus,
    ) -> Result<()> {
        let conn = Arc::clone(&self.conn);
        let run_id = run_id.to_string();
        let status = status.to_string();

        flatten_join(
            tokio::task::spawn_blocking(move || {
                let conn = conn
                    .lock()
                    .map_err(|e| IronCrewError::Validation(format!("SQLite lock error: {}", e)))?;
                let rows = conn
                    .execute(
                        "UPDATE runs SET status = ?1
                         WHERE run_id = ?2 AND status IN ('running', 'waiting_for_input')",
                        rusqlite::params![status, run_id],
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

    async fn reconcile_abandoned_runs(&self, now: &str) -> Result<usize> {
        let conn = Arc::clone(&self.conn);
        let now = now.to_string();

        flatten_join(
            tokio::task::spawn_blocking(move || {
                let conn = conn
                    .lock()
                    .map_err(|e| IronCrewError::Validation(format!("SQLite lock error: {}", e)))?;
                let rows = conn
                    .execute(
                        "UPDATE runs SET status = 'abandoned', finished_at = ?1 WHERE status IN ('running', 'waiting_for_input')",
                        rusqlite::params![now],
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
                        "SELECT run_id, flow_name, flow, status, started_at, finished_at, duration_ms, task_results, agent_count, task_count, total_tokens, cached_tokens, tags FROM runs WHERE run_id = ?1",
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
                                task_results: serde_json::from_str(&task_results_json)
                                    .unwrap_or_default(),
                                agent_count: row.get::<_, i64>(8)? as usize,
                                task_count: row.get::<_, i64>(9)? as usize,
                                total_tokens: row.get::<_, i64>(10)? as u32,
                                cached_tokens: row.get::<_, i64>(11)? as u32,
                                tags: serde_json::from_str(&tags_json).unwrap_or_default(),
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
                                tags: serde_json::from_str(&tags_json).unwrap_or_default(),
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

    async fn save_conversation(&self, record: &ConversationRecord) -> Result<()> {
        let conn = Arc::clone(&self.conn);
        let record = record.clone();

        flatten_join(
            tokio::task::spawn_blocking(move || {
                let conn = conn
                    .lock()
                    .map_err(|e| IronCrewError::Validation(format!("SQLite lock error: {}", e)))?;

                let messages_json = serde_json::to_string(&record.messages).map_err(|e| {
                    IronCrewError::Validation(format!("Failed to serialize messages: {}", e))
                })?;

                // SQLite's `UNIQUE(flow_path, id)` does not consider `NULL`
                // values equal, so legacy/global records need an explicit
                // delete-first step to preserve the store's upsert contract.
                if record.flow_path.is_none() {
                    conn.execute(
                        "DELETE FROM conversations WHERE id = ?1 AND flow_path IS NULL",
                        rusqlite::params![record.id],
                    )
                    .map_err(|e| {
                        IronCrewError::Validation(format!(
                            "SQLite save_conversation delete-old-null-scope error: {}",
                            e
                        ))
                    })?;
                }

                conn.execute(
                    "INSERT OR REPLACE INTO conversations \
                     (id, flow_name, flow_path, agent_name, messages, created_at, updated_at) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    rusqlite::params![
                        record.id,
                        record.flow_name,
                        record.flow_path,
                        record.agent_name,
                        messages_json,
                        record.created_at,
                        record.updated_at,
                    ],
                )
                .map_err(|e| {
                    IronCrewError::Validation(format!("SQLite save_conversation error: {}", e))
                })?;
                Ok(())
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
                        "SELECT id, flow_name, flow_path, agent_name, messages, created_at, updated_at \
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
                            messages: serde_json::from_str(&messages_json).unwrap_or_default(),
                            created_at: row.get(5)?,
                            updated_at: row.get(6)?,
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
                    "SELECT id, flow_path, agent_name, messages, created_at, updated_at \
                     FROM conversations",
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
                        let messages_json: String = row.get(3)?;
                        let msgs: Vec<crate::llm::provider::ChatMessage> =
                            serde_json::from_str(&messages_json).unwrap_or_default();
                        let turn_count = msgs.iter().filter(|m| m.role == "user").count();
                        Ok(ConversationSummary {
                            id: row.get(0)?,
                            flow_path: row.get(1)?,
                            agent_name: row.get(2)?,
                            created_at: row.get(4)?,
                            updated_at: row.get(5)?,
                            turn_count,
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

    async fn save_dialog_state(&self, record: &DialogStateRecord) -> Result<()> {
        let conn = Arc::clone(&self.conn);
        let record = record.clone();

        flatten_join(
            tokio::task::spawn_blocking(move || {
                let conn = conn
                    .lock()
                    .map_err(|e| IronCrewError::Validation(format!("SQLite lock error: {}", e)))?;

                let agents_json = serde_json::to_string(&record.agent_names).map_err(|e| {
                    IronCrewError::Validation(format!("Failed to serialize agent_names: {}", e))
                })?;
                let transcript_json = serde_json::to_string(&record.transcript).map_err(|e| {
                    IronCrewError::Validation(format!("Failed to serialize transcript: {}", e))
                })?;

                // See `save_conversation`: NULL-scoped legacy/global records need
                // an explicit delete to preserve replace semantics on SQLite.
                if record.flow_path.is_none() {
                    conn.execute(
                        "DELETE FROM dialogs WHERE id = ?1 AND flow_path IS NULL",
                        rusqlite::params![record.id],
                    )
                    .map_err(|e| {
                        IronCrewError::Validation(format!(
                            "SQLite save_dialog_state delete-old-null-scope error: {}",
                            e
                        ))
                    })?;
                }

                conn.execute(
                    "INSERT OR REPLACE INTO dialogs \
                     (id, flow_name, flow_path, agent_names, starter, transcript, next_index, stopped, stop_reason, created_at, updated_at) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                    rusqlite::params![
                        record.id,
                        record.flow_name,
                        record.flow_path,
                        agents_json,
                        record.starter,
                        transcript_json,
                        record.next_index as i64,
                        record.stopped as i64,
                        record.stop_reason,
                        record.created_at,
                        record.updated_at,
                    ],
                )
                .map_err(|e| {
                    IronCrewError::Validation(format!("SQLite save_dialog_state error: {}", e))
                })?;
                Ok(())
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
                         stopped, stop_reason, created_at, updated_at \
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
                            agent_names: serde_json::from_str(&agents_json).unwrap_or_default(),
                            starter: row.get(4)?,
                            transcript: serde_json::from_str(&transcript_json).unwrap_or_default(),
                            next_index: row.get::<_, i64>(6)? as usize,
                            stopped: row.get::<_, i64>(7)? != 0,
                            stop_reason: row.get(8)?,
                            created_at: row.get(9)?,
                            updated_at: row.get(10)?,
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
                            metadata: metadata_json.and_then(|s| serde_json::from_str(&s).ok()),
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
