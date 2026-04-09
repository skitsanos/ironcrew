use async_trait::async_trait;
use rusqlite::Connection;
use std::path::PathBuf;
use std::sync::Mutex;

use super::run_history::{ListRunsFilter, RunRecord, RunStatus, RunSummary};
use super::sessions::{ConversationRecord, DialogStateRecord};
use super::store::StateStore;
use crate::utils::error::{IronCrewError, Result};

pub struct SqliteStore {
    conn: Mutex<Connection>,
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
            );",
        )
        .map_err(|e| IronCrewError::Validation(format!("Failed to create SQLite tables: {}", e)))?;

        Ok(Self {
            conn: Mutex::new(conn),
        })
    }
}

#[async_trait]
impl StateStore for SqliteStore {
    async fn save_run(&self, record: &RunRecord) -> Result<String> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| IronCrewError::Validation(format!("SQLite lock error: {}", e)))?;

        let task_results_json = serde_json::to_string(&record.task_results).map_err(|e| {
            IronCrewError::Validation(format!("Failed to serialize task_results: {}", e))
        })?;
        let tags_json = serde_json::to_string(&record.tags)
            .map_err(|e| IronCrewError::Validation(format!("Failed to serialize tags: {}", e)))?;

        conn.execute(
            "INSERT OR REPLACE INTO runs (run_id, flow_name, status, started_at, finished_at, duration_ms, task_results, agent_count, task_count, total_tokens, cached_tokens, tags)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            rusqlite::params![
                record.run_id,
                record.flow_name,
                record.status.to_string(),
                record.started_at,
                record.finished_at,
                record.duration_ms as i64,
                task_results_json,
                record.agent_count as i64,
                record.task_count as i64,
                record.total_tokens as i64,
                record.cached_tokens as i64,
                tags_json,
            ],
        )
        .map_err(|e| IronCrewError::Validation(format!("SQLite insert error: {}", e)))?;

        tracing::info!("Run saved to SQLite: {}", record.run_id);
        Ok(record.run_id.clone())
    }

    async fn get_run(&self, run_id: &str) -> Result<RunRecord> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| IronCrewError::Validation(format!("SQLite lock error: {}", e)))?;

        let mut stmt = conn
            .prepare(
                "SELECT run_id, flow_name, status, started_at, finished_at, duration_ms, task_results, agent_count, task_count, total_tokens, cached_tokens, tags FROM runs WHERE run_id = ?1",
            )
            .map_err(|e| IronCrewError::Validation(format!("SQLite prepare error: {}", e)))?;

        let record = stmt
            .query_row(rusqlite::params![run_id], |row| {
                let status_str: String = row.get(2)?;
                let task_results_json: String = row.get(6)?;
                let tags_json: String = row.get(11)?;

                Ok(RunRecord {
                    run_id: row.get(0)?,
                    flow_name: row.get(1)?,
                    status: match status_str.as_str() {
                        "success" => RunStatus::Success,
                        "partial_failure" => RunStatus::PartialFailure,
                        _ => RunStatus::Failed,
                    },
                    started_at: row.get(3)?,
                    finished_at: row.get(4)?,
                    duration_ms: row.get::<_, i64>(5)? as u64,
                    task_results: serde_json::from_str(&task_results_json).unwrap_or_default(),
                    agent_count: row.get::<_, i64>(7)? as usize,
                    task_count: row.get::<_, i64>(8)? as usize,
                    total_tokens: row.get::<_, i64>(9)? as u32,
                    cached_tokens: row.get::<_, i64>(10)? as u32,
                    tags: serde_json::from_str(&tags_json).unwrap_or_default(),
                })
            })
            .map_err(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => {
                    IronCrewError::Validation(format!("Run '{}' not found", run_id))
                }
                _ => IronCrewError::Validation(format!("SQLite query error: {}", e)),
            })?;

        Ok(record)
    }

    async fn list_runs_summary(
        &self,
        filter: &ListRunsFilter,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<RunSummary>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| IronCrewError::Validation(format!("SQLite lock error: {}", e)))?;

        // Build WHERE clause dynamically. NOTE: we never select task_results.
        let mut sql = String::from(
            "SELECT run_id, flow_name, status, started_at, finished_at, duration_ms, \
             agent_count, task_count, total_tokens, cached_tokens, tags \
             FROM runs",
        );
        let mut where_clauses: Vec<String> = Vec::new();
        let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
        let mut next_idx = 1usize;

        if let Some(ref status) = filter.status {
            where_clauses.push(format!("status = ?{}", next_idx));
            params.push(Box::new(status.clone()));
            next_idx += 1;
        }
        if let Some(ref since) = filter.since {
            where_clauses.push(format!("started_at >= ?{}", next_idx));
            params.push(Box::new(since.clone()));
            next_idx += 1;
        }
        // Tag filter uses LIKE on the JSON text — good enough for small tag
        // sets. Quotes are added so "foo" doesn't accidentally match "foobar".
        if let Some(ref tag) = filter.tag {
            where_clauses.push(format!("tags LIKE ?{}", next_idx));
            params.push(Box::new(format!("%\"{}\"%", tag)));
            next_idx += 1;
        }
        if !where_clauses.is_empty() {
            sql.push_str(" WHERE ");
            sql.push_str(&where_clauses.join(" AND "));
        }
        sql.push_str(" ORDER BY started_at DESC");

        if limit > 0 {
            sql.push_str(&format!(" LIMIT ?{}", next_idx));
            params.push(Box::new(limit as i64));
            next_idx += 1;
            if offset > 0 {
                sql.push_str(&format!(" OFFSET ?{}", next_idx));
                params.push(Box::new(offset as i64));
            }
        }

        let mut stmt = conn
            .prepare(&sql)
            .map_err(|e| IronCrewError::Validation(format!("SQLite prepare error: {}", e)))?;

        let param_refs: Vec<&dyn rusqlite::types::ToSql> =
            params.iter().map(|p| p.as_ref()).collect();

        let rows = stmt
            .query_map(param_refs.as_slice(), |row| {
                let status_str: String = row.get(2)?;
                let tags_json: String = row.get(10)?;
                Ok(RunSummary {
                    run_id: row.get(0)?,
                    flow_name: row.get(1)?,
                    status: match status_str.as_str() {
                        "success" => RunStatus::Success,
                        "partial_failure" => RunStatus::PartialFailure,
                        _ => RunStatus::Failed,
                    },
                    started_at: row.get(3)?,
                    finished_at: row.get(4)?,
                    duration_ms: row.get::<_, i64>(5)? as u64,
                    agent_count: row.get::<_, i64>(6)? as usize,
                    task_count: row.get::<_, i64>(7)? as usize,
                    total_tokens: row.get::<_, i64>(8)? as u32,
                    cached_tokens: row.get::<_, i64>(9)? as u32,
                    tags: serde_json::from_str(&tags_json).unwrap_or_default(),
                })
            })
            .map_err(|e| IronCrewError::Validation(format!("SQLite query error: {}", e)))?;

        let mut summaries = Vec::new();
        for summary in rows.flatten() {
            summaries.push(summary);
        }
        Ok(summaries)
    }

    async fn count_runs(&self, filter: &ListRunsFilter) -> Result<u64> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| IronCrewError::Validation(format!("SQLite lock error: {}", e)))?;

        let mut sql = String::from("SELECT COUNT(*) FROM runs");
        let mut where_clauses: Vec<String> = Vec::new();
        let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
        let mut next_idx = 1usize;

        if let Some(ref status) = filter.status {
            where_clauses.push(format!("status = ?{}", next_idx));
            params.push(Box::new(status.clone()));
            next_idx += 1;
        }
        if let Some(ref since) = filter.since {
            where_clauses.push(format!("started_at >= ?{}", next_idx));
            params.push(Box::new(since.clone()));
            next_idx += 1;
        }
        if let Some(ref tag) = filter.tag {
            where_clauses.push(format!("tags LIKE ?{}", next_idx));
            params.push(Box::new(format!("%\"{}\"%", tag)));
        }
        if !where_clauses.is_empty() {
            sql.push_str(" WHERE ");
            sql.push_str(&where_clauses.join(" AND "));
        }

        let param_refs: Vec<&dyn rusqlite::types::ToSql> =
            params.iter().map(|p| p.as_ref()).collect();
        let count: i64 = conn
            .query_row(&sql, param_refs.as_slice(), |row| row.get(0))
            .map_err(|e| IronCrewError::Validation(format!("SQLite count error: {}", e)))?;
        Ok(count as u64)
    }

    async fn delete_run(&self, run_id: &str) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| IronCrewError::Validation(format!("SQLite lock error: {}", e)))?;

        let affected = conn
            .execute(
                "DELETE FROM runs WHERE run_id = ?1",
                rusqlite::params![run_id],
            )
            .map_err(|e| IronCrewError::Validation(format!("SQLite delete error: {}", e)))?;

        if affected == 0 {
            return Err(IronCrewError::Validation(format!(
                "Run '{}' not found",
                run_id
            )));
        }
        Ok(())
    }

    // ─── Persistent sessions ────────────────────────────────────────────────

    async fn save_conversation(&self, record: &ConversationRecord) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| IronCrewError::Validation(format!("SQLite lock error: {}", e)))?;

        let messages_json = serde_json::to_string(&record.messages).map_err(|e| {
            IronCrewError::Validation(format!("Failed to serialize messages: {}", e))
        })?;

        conn.execute(
            "INSERT OR REPLACE INTO conversations \
             (id, flow_name, agent_name, messages, created_at, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![
                record.id,
                record.flow_name,
                record.agent_name,
                messages_json,
                record.created_at,
                record.updated_at,
            ],
        )
        .map_err(|e| IronCrewError::Validation(format!("SQLite save_conversation error: {}", e)))?;
        Ok(())
    }

    async fn get_conversation(&self, id: &str) -> Result<Option<ConversationRecord>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| IronCrewError::Validation(format!("SQLite lock error: {}", e)))?;

        let mut stmt = conn
            .prepare(
                "SELECT id, flow_name, agent_name, messages, created_at, updated_at \
                 FROM conversations WHERE id = ?1",
            )
            .map_err(|e| IronCrewError::Validation(format!("SQLite prepare error: {}", e)))?;

        let row = stmt
            .query_row(rusqlite::params![id], |row| {
                let messages_json: String = row.get(3)?;
                Ok(ConversationRecord {
                    id: row.get(0)?,
                    flow_name: row.get(1)?,
                    agent_name: row.get(2)?,
                    messages: serde_json::from_str(&messages_json).unwrap_or_default(),
                    created_at: row.get(4)?,
                    updated_at: row.get(5)?,
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
    }

    async fn delete_conversation(&self, id: &str) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| IronCrewError::Validation(format!("SQLite lock error: {}", e)))?;
        conn.execute(
            "DELETE FROM conversations WHERE id = ?1",
            rusqlite::params![id],
        )
        .map_err(|e| {
            IronCrewError::Validation(format!("SQLite delete_conversation error: {}", e))
        })?;
        Ok(())
    }

    async fn save_dialog_state(&self, record: &DialogStateRecord) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| IronCrewError::Validation(format!("SQLite lock error: {}", e)))?;

        let agents_json = serde_json::to_string(&record.agent_names).map_err(|e| {
            IronCrewError::Validation(format!("Failed to serialize agent_names: {}", e))
        })?;
        let transcript_json = serde_json::to_string(&record.transcript).map_err(|e| {
            IronCrewError::Validation(format!("Failed to serialize transcript: {}", e))
        })?;

        conn.execute(
            "INSERT OR REPLACE INTO dialogs \
             (id, flow_name, agent_names, starter, transcript, next_index, stopped, stop_reason, created_at, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            rusqlite::params![
                record.id,
                record.flow_name,
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
    }

    async fn get_dialog_state(&self, id: &str) -> Result<Option<DialogStateRecord>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| IronCrewError::Validation(format!("SQLite lock error: {}", e)))?;

        let mut stmt = conn
            .prepare(
                "SELECT id, flow_name, agent_names, starter, transcript, next_index, \
                 stopped, stop_reason, created_at, updated_at \
                 FROM dialogs WHERE id = ?1",
            )
            .map_err(|e| IronCrewError::Validation(format!("SQLite prepare error: {}", e)))?;

        let row = stmt
            .query_row(rusqlite::params![id], |row| {
                let agents_json: String = row.get(2)?;
                let transcript_json: String = row.get(4)?;
                Ok(DialogStateRecord {
                    id: row.get(0)?,
                    flow_name: row.get(1)?,
                    agent_names: serde_json::from_str(&agents_json).unwrap_or_default(),
                    starter: row.get(3)?,
                    transcript: serde_json::from_str(&transcript_json).unwrap_or_default(),
                    next_index: row.get::<_, i64>(5)? as usize,
                    stopped: row.get::<_, i64>(6)? != 0,
                    stop_reason: row.get(7)?,
                    created_at: row.get(8)?,
                    updated_at: row.get(9)?,
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
    }

    async fn delete_dialog_state(&self, id: &str) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| IronCrewError::Validation(format!("SQLite lock error: {}", e)))?;
        conn.execute("DELETE FROM dialogs WHERE id = ?1", rusqlite::params![id])
            .map_err(|e| {
                IronCrewError::Validation(format!("SQLite delete_dialog_state error: {}", e))
            })?;
        Ok(())
    }
}
