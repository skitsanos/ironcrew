#![cfg(feature = "postgres")]

use async_trait::async_trait;
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Row};

use super::run_history::{ListRunsFilter, RunRecord, RunStatus, RunSummary};
use super::sessions::{ConversationRecord, DialogStateRecord};
use super::store::StateStore;
use crate::utils::error::{IronCrewError, Result};

pub struct PostgresStore {
    pool: PgPool,
    table_name: String,
    conversations_table: String,
    dialogs_table: String,
}

impl PostgresStore {
    /// Create a new PostgreSQL store.
    /// `table_prefix` allows sharing a database across projects:
    ///   prefix = "myapp_" → table = "myapp_runs"
    ///   prefix = "" → table = "runs" (default)
    pub async fn new(database_url: &str, table_prefix: &str) -> Result<Self> {
        // Validate table prefix to prevent SQL injection via env var
        if !table_prefix
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            return Err(IronCrewError::Validation(format!(
                "Invalid IRONCREW_PG_TABLE_PREFIX '{}': only alphanumeric and underscore allowed",
                table_prefix
            )));
        }

        let max_conn: u32 = std::env::var("IRONCREW_DB_POOL_SIZE")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(10);

        let pool = PgPoolOptions::new()
            .max_connections(max_conn)
            .connect(database_url)
            .await
            .map_err(|e| {
                IronCrewError::Validation(format!("Failed to connect to PostgreSQL: {}", e))
            })?;

        let table_name = format!("{}runs", table_prefix);
        let conversations_table = format!("{}conversations", table_prefix);
        let dialogs_table = format!("{}dialogs", table_prefix);

        let store = Self {
            pool,
            table_name: table_name.clone(),
            conversations_table,
            dialogs_table,
        };
        store.bootstrap().await?;

        tracing::info!("PostgreSQL store ready (table: {})", table_name);
        Ok(store)
    }

    /// Bootstrap the database: create table, add missing columns, fix types, create indexes.
    async fn bootstrap(&self) -> Result<()> {
        let t = &self.table_name;

        // 1. Create table if not exists
        let create_sql = format!(
            "CREATE TABLE IF NOT EXISTS {t} (
                run_id        TEXT PRIMARY KEY,
                flow_name     TEXT NOT NULL,
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
                created_at    TIMESTAMPTZ DEFAULT NOW()
            )"
        );
        sqlx::query(&create_sql)
            .execute(&self.pool)
            .await
            .map_err(|e| IronCrewError::Validation(format!("Failed to create {t} table: {e}")))?;

        // 2. Add missing columns (heal older schema versions)
        let migrations: &[(&str, &str)] = &[
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
            if let Err(e) = sqlx::query(sql).execute(&self.pool).await {
                tracing::warn!("Migration for column '{}': {}", col, e);
            }
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
            if let Err(e) = sqlx::query(sql).execute(&self.pool).await {
                tracing::warn!("Type fix for column '{}': {}", col, e);
            }
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
        ];

        for sql in indexes {
            if let Err(e) = sqlx::query(sql).execute(&self.pool).await {
                tracing::warn!("Index creation: {}", e);
            }
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
                    updated_at  TEXT NOT NULL
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
                    updated_at  TEXT NOT NULL
                )"
            ),
        ];
        for sql in &session_tables {
            sqlx::query(sql).execute(&self.pool).await.map_err(|e| {
                IronCrewError::Validation(format!("Failed to create session table: {}", e))
            })?;
        }

        // Session indexes — updated_at helps "list recent sessions" queries
        let session_indexes = [
            format!("CREATE INDEX IF NOT EXISTS idx_{ct}_updated_at ON {ct} (updated_at DESC)"),
            format!("CREATE INDEX IF NOT EXISTS idx_{ct}_flow_name ON {ct} (flow_name)"),
            format!("CREATE INDEX IF NOT EXISTS idx_{dt}_updated_at ON {dt} (updated_at DESC)"),
            format!("CREATE INDEX IF NOT EXISTS idx_{dt}_flow_name ON {dt} (flow_name)"),
        ];
        for sql in &session_indexes {
            if let Err(e) = sqlx::query(sql).execute(&self.pool).await {
                tracing::warn!("Session index creation: {}", e);
            }
        }

        tracing::debug!(
            "PostgreSQL bootstrap complete for tables '{}', '{}', '{}'",
            self.table_name,
            self.conversations_table,
            self.dialogs_table
        );
        Ok(())
    }
}

#[async_trait]
impl StateStore for PostgresStore {
    async fn save_run(&self, record: &RunRecord) -> Result<String> {
        let task_results_json = serde_json::to_string(&record.task_results).map_err(|e| {
            IronCrewError::Validation(format!("Failed to serialize task_results: {}", e))
        })?;
        let tags_json = serde_json::to_string(&record.tags)
            .map_err(|e| IronCrewError::Validation(format!("Failed to serialize tags: {}", e)))?;

        let sql = format!(
            "INSERT INTO {} (run_id, flow_name, status, started_at, finished_at, duration_ms, task_results, agent_count, task_count, total_tokens, cached_tokens, tags)
             VALUES ($1, $2, $3, $4, $5, $6, $7::jsonb, $8, $9, $10, $11, $12::jsonb)
             ON CONFLICT (run_id) DO UPDATE SET
                flow_name = EXCLUDED.flow_name,
                status = EXCLUDED.status,
                started_at = EXCLUDED.started_at,
                finished_at = EXCLUDED.finished_at,
                duration_ms = EXCLUDED.duration_ms,
                task_results = EXCLUDED.task_results,
                agent_count = EXCLUDED.agent_count,
                task_count = EXCLUDED.task_count,
                total_tokens = EXCLUDED.total_tokens,
                cached_tokens = EXCLUDED.cached_tokens,
                tags = EXCLUDED.tags",
            self.table_name
        );

        sqlx::query(&sql)
            .bind(&record.run_id)
            .bind(&record.flow_name)
            .bind(record.status.to_string())
            .bind(&record.started_at)
            .bind(&record.finished_at)
            .bind(record.duration_ms as i64)
            .bind(&task_results_json)
            .bind(record.agent_count as i32)
            .bind(record.task_count as i32)
            .bind(record.total_tokens as i32)
            .bind(record.cached_tokens as i32)
            .bind(&tags_json)
            .execute(&self.pool)
            .await
            .map_err(|e| IronCrewError::Validation(format!("PostgreSQL insert error: {}", e)))?;

        tracing::info!("Run saved to PostgreSQL: {}", record.run_id);
        Ok(record.run_id.clone())
    }

    async fn get_run(&self, run_id: &str) -> Result<RunRecord> {
        let sql = format!(
            "SELECT run_id, flow_name, status, started_at, finished_at, duration_ms, task_results::text, agent_count, task_count, total_tokens, cached_tokens, tags::text
             FROM {} WHERE run_id = $1",
            self.table_name
        );

        let row = sqlx::query(&sql)
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
        // Build WHERE clause dynamically with numbered placeholders.
        // Note: we NEVER select task_results — that's the whole point of
        // the summary view. Without the heavy JSONB column, this query is
        // effectively just an index scan on started_at.
        let mut sql = format!(
            "SELECT run_id, flow_name, status, started_at, finished_at, duration_ms, \
             agent_count, task_count, total_tokens, cached_tokens, tags::text \
             FROM {}",
            self.table_name
        );
        let mut where_clauses: Vec<String> = Vec::new();
        let mut next_idx = 1usize;

        if filter.status.is_some() {
            where_clauses.push(format!("status = ${}", next_idx));
            next_idx += 1;
        }
        if filter.since.is_some() {
            where_clauses.push(format!("started_at >= ${}", next_idx));
            next_idx += 1;
        }
        if filter.tag.is_some() {
            // JSONB @> for containment — uses the GIN index on tags
            where_clauses.push(format!("tags @> ${}::jsonb", next_idx));
            next_idx += 1;
        }
        if !where_clauses.is_empty() {
            sql.push_str(" WHERE ");
            sql.push_str(&where_clauses.join(" AND "));
        }
        sql.push_str(" ORDER BY started_at DESC");

        if limit > 0 {
            sql.push_str(&format!(" LIMIT ${}", next_idx));
            next_idx += 1;
            if offset > 0 {
                sql.push_str(&format!(" OFFSET ${}", next_idx));
            }
        }

        // Bind parameters in the same order they appear in the SQL
        let mut query = sqlx::query(&sql);
        if let Some(ref status) = filter.status {
            query = query.bind(status);
        }
        if let Some(ref since) = filter.since {
            query = query.bind(since);
        }
        if let Some(ref tag) = filter.tag {
            // Wrap in a JSONB array: ["tag"]
            query = query.bind(format!("[\"{}\"]", tag));
        }
        if limit > 0 {
            query = query.bind(limit as i64);
            if offset > 0 {
                query = query.bind(offset as i64);
            }
        }

        let rows = query
            .fetch_all(&self.pool)
            .await
            .map_err(|e| IronCrewError::Validation(format!("PostgreSQL query error: {}", e)))?;

        rows.iter().map(row_to_summary).collect()
    }

    async fn count_runs(&self, filter: &ListRunsFilter) -> Result<u64> {
        let mut sql = format!("SELECT COUNT(*) FROM {}", self.table_name);
        let mut where_clauses: Vec<String> = Vec::new();
        let mut next_idx = 1usize;

        if filter.status.is_some() {
            where_clauses.push(format!("status = ${}", next_idx));
            next_idx += 1;
        }
        if filter.since.is_some() {
            where_clauses.push(format!("started_at >= ${}", next_idx));
            next_idx += 1;
        }
        if filter.tag.is_some() {
            where_clauses.push(format!("tags @> ${}::jsonb", next_idx));
        }
        if !where_clauses.is_empty() {
            sql.push_str(" WHERE ");
            sql.push_str(&where_clauses.join(" AND "));
        }

        let mut query = sqlx::query(&sql);
        if let Some(ref status) = filter.status {
            query = query.bind(status);
        }
        if let Some(ref since) = filter.since {
            query = query.bind(since);
        }
        if let Some(ref tag) = filter.tag {
            query = query.bind(format!("[\"{}\"]", tag));
        }

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

        let result = sqlx::query(&sql)
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

    async fn save_conversation(&self, record: &ConversationRecord) -> Result<()> {
        let messages_json = serde_json::to_string(&record.messages).map_err(|e| {
            IronCrewError::Validation(format!("Failed to serialize messages: {}", e))
        })?;
        let sql = format!(
            "INSERT INTO {t} (id, flow_name, agent_name, messages, created_at, updated_at) \
             VALUES ($1, $2, $3, $4::jsonb, $5, $6) \
             ON CONFLICT (id) DO UPDATE SET \
               flow_name = EXCLUDED.flow_name, \
               agent_name = EXCLUDED.agent_name, \
               messages = EXCLUDED.messages, \
               updated_at = EXCLUDED.updated_at",
            t = self.conversations_table
        );
        sqlx::query(&sql)
            .bind(&record.id)
            .bind(&record.flow_name)
            .bind(&record.agent_name)
            .bind(&messages_json)
            .bind(&record.created_at)
            .bind(&record.updated_at)
            .execute(&self.pool)
            .await
            .map_err(|e| {
                IronCrewError::Validation(format!("PostgreSQL save_conversation error: {}", e))
            })?;
        Ok(())
    }

    async fn get_conversation(&self, id: &str) -> Result<Option<ConversationRecord>> {
        let sql = format!(
            "SELECT id, flow_name, agent_name, messages::text, created_at, updated_at \
             FROM {} WHERE id = $1",
            self.conversations_table
        );
        let row_opt = sqlx::query(&sql)
            .bind(id)
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
            agent_name: row
                .try_get("agent_name")
                .map_err(|e| IronCrewError::Validation(e.to_string()))?,
            messages: serde_json::from_str(&messages_str).unwrap_or_default(),
            created_at: row
                .try_get("created_at")
                .map_err(|e| IronCrewError::Validation(e.to_string()))?,
            updated_at: row
                .try_get("updated_at")
                .map_err(|e| IronCrewError::Validation(e.to_string()))?,
        }))
    }

    async fn delete_conversation(&self, id: &str) -> Result<()> {
        let sql = format!("DELETE FROM {} WHERE id = $1", self.conversations_table);
        sqlx::query(&sql)
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(|e| {
                IronCrewError::Validation(format!("PostgreSQL delete_conversation error: {}", e))
            })?;
        Ok(())
    }

    async fn save_dialog_state(&self, record: &DialogStateRecord) -> Result<()> {
        let agents_json = serde_json::to_string(&record.agent_names).map_err(|e| {
            IronCrewError::Validation(format!("Failed to serialize agent_names: {}", e))
        })?;
        let transcript_json = serde_json::to_string(&record.transcript).map_err(|e| {
            IronCrewError::Validation(format!("Failed to serialize transcript: {}", e))
        })?;
        let sql = format!(
            "INSERT INTO {t} \
             (id, flow_name, agent_names, starter, transcript, next_index, stopped, stop_reason, created_at, updated_at) \
             VALUES ($1, $2, $3::jsonb, $4, $5::jsonb, $6, $7, $8, $9, $10) \
             ON CONFLICT (id) DO UPDATE SET \
               flow_name = EXCLUDED.flow_name, \
               agent_names = EXCLUDED.agent_names, \
               starter = EXCLUDED.starter, \
               transcript = EXCLUDED.transcript, \
               next_index = EXCLUDED.next_index, \
               stopped = EXCLUDED.stopped, \
               stop_reason = EXCLUDED.stop_reason, \
               updated_at = EXCLUDED.updated_at",
            t = self.dialogs_table
        );
        sqlx::query(&sql)
            .bind(&record.id)
            .bind(&record.flow_name)
            .bind(&agents_json)
            .bind(&record.starter)
            .bind(&transcript_json)
            .bind(record.next_index as i32)
            .bind(record.stopped)
            .bind(&record.stop_reason)
            .bind(&record.created_at)
            .bind(&record.updated_at)
            .execute(&self.pool)
            .await
            .map_err(|e| {
                IronCrewError::Validation(format!("PostgreSQL save_dialog_state error: {}", e))
            })?;
        Ok(())
    }

    async fn get_dialog_state(&self, id: &str) -> Result<Option<DialogStateRecord>> {
        let sql = format!(
            "SELECT id, flow_name, agent_names::text, starter, transcript::text, \
             next_index, stopped, stop_reason, created_at, updated_at \
             FROM {} WHERE id = $1",
            self.dialogs_table
        );
        let row_opt = sqlx::query(&sql)
            .bind(id)
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
            agent_names: serde_json::from_str(&agents_str).unwrap_or_default(),
            starter: row
                .try_get("starter")
                .map_err(|e| IronCrewError::Validation(e.to_string()))?,
            transcript: serde_json::from_str(&transcript_str).unwrap_or_default(),
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
        }))
    }

    async fn delete_dialog_state(&self, id: &str) -> Result<()> {
        let sql = format!("DELETE FROM {} WHERE id = $1", self.dialogs_table);
        sqlx::query(&sql)
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(|e| {
                IronCrewError::Validation(format!("PostgreSQL delete_dialog_state error: {}", e))
            })?;
        Ok(())
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
        status: match status_str.as_str() {
            "success" => RunStatus::Success,
            "partial_failure" => RunStatus::PartialFailure,
            _ => RunStatus::Failed,
        },
        started_at: row
            .try_get("started_at")
            .map_err(|e| IronCrewError::Validation(format!("Column error: {}", e)))?,
        finished_at: row
            .try_get("finished_at")
            .map_err(|e| IronCrewError::Validation(format!("Column error: {}", e)))?,
        duration_ms: duration_ms as u64,
        task_results: serde_json::from_str(&task_results_str).unwrap_or_default(),
        agent_count: agent_count as usize,
        task_count: task_count as usize,
        total_tokens: total_tokens as u32,
        cached_tokens: cached_tokens as u32,
        tags: serde_json::from_str(&tags_str).unwrap_or_default(),
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
        status: match status_str.as_str() {
            "success" => RunStatus::Success,
            "partial_failure" => RunStatus::PartialFailure,
            _ => RunStatus::Failed,
        },
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
        tags: serde_json::from_str(&tags_str).unwrap_or_default(),
    })
}
