#![cfg(feature = "postgres")]

use async_trait::async_trait;
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Row};

use super::run_history::{RunRecord, RunStatus};
use super::store::StateStore;
use crate::utils::error::{IronCrewError, Result};

pub struct PostgresStore {
    pool: PgPool,
}

impl PostgresStore {
    pub async fn new(database_url: &str) -> Result<Self> {
        let pool = PgPoolOptions::new()
            .max_connections(5)
            .connect(database_url)
            .await
            .map_err(|e| {
                IronCrewError::Validation(format!("Failed to connect to PostgreSQL: {}", e))
            })?;

        // Create table if not exists
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS runs (
                run_id        TEXT PRIMARY KEY,
                flow_name     TEXT NOT NULL,
                status        TEXT NOT NULL,
                started_at    TEXT NOT NULL,
                finished_at   TEXT NOT NULL,
                duration_ms   BIGINT NOT NULL,
                task_results  TEXT NOT NULL,
                agent_count   INTEGER NOT NULL,
                task_count    INTEGER NOT NULL,
                total_tokens  INTEGER DEFAULT 0,
                cached_tokens INTEGER DEFAULT 0,
                tags          TEXT DEFAULT '[]',
                created_at    TIMESTAMPTZ DEFAULT NOW()
            )",
        )
        .execute(&pool)
        .await
        .map_err(|e| IronCrewError::Validation(format!("Failed to create runs table: {}", e)))?;

        tracing::info!("PostgreSQL store connected");
        Ok(Self { pool })
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

        sqlx::query(
            "INSERT INTO runs (run_id, flow_name, status, started_at, finished_at, duration_ms, task_results, agent_count, task_count, total_tokens, cached_tokens, tags)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)
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
        )
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
        let row = sqlx::query(
            "SELECT run_id, flow_name, status, started_at, finished_at, duration_ms, task_results, agent_count, task_count, total_tokens, cached_tokens, tags
             FROM runs WHERE run_id = $1",
        )
        .bind(run_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| IronCrewError::Validation(format!("PostgreSQL query error: {}", e)))?
        .ok_or_else(|| IronCrewError::Validation(format!("Run '{}' not found", run_id)))?;

        row_to_record(&row)
    }

    async fn list_runs(&self, status_filter: Option<&str>) -> Result<Vec<RunRecord>> {
        let rows = if let Some(filter) = status_filter {
            sqlx::query(
                "SELECT run_id, flow_name, status, started_at, finished_at, duration_ms, task_results, agent_count, task_count, total_tokens, cached_tokens, tags
                 FROM runs WHERE status = $1 ORDER BY started_at DESC",
            )
            .bind(filter)
            .fetch_all(&self.pool)
            .await
        } else {
            sqlx::query(
                "SELECT run_id, flow_name, status, started_at, finished_at, duration_ms, task_results, agent_count, task_count, total_tokens, cached_tokens, tags
                 FROM runs ORDER BY started_at DESC",
            )
            .fetch_all(&self.pool)
            .await
        }
        .map_err(|e| IronCrewError::Validation(format!("PostgreSQL query error: {}", e)))?;

        rows.iter().map(row_to_record).collect()
    }

    async fn delete_run(&self, run_id: &str) -> Result<()> {
        let result = sqlx::query("DELETE FROM runs WHERE run_id = $1")
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
