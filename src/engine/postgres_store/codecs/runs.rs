use sqlx::{Row, postgres::PgRow};

use crate::engine::run_history::{RunRecord, RunStatus, RunSummary};
use crate::utils::error::{IronCrewError, Result};

use super::decode_stored_json;

pub(in crate::engine::postgres_store) fn run_record(row: &PgRow) -> Result<RunRecord> {
    let status_str: String = column(row, "status")?;
    let task_results_str: String = column(row, "task_results")?;
    let tags_str: String = column(row, "tags")?;
    let duration_ms: i64 = column(row, "duration_ms")?;
    let agent_count: i32 = column(row, "agent_count")?;
    let task_count: i32 = column(row, "task_count")?;
    let total_tokens: i32 = column(row, "total_tokens")?;
    let cached_tokens: i32 = column(row, "cached_tokens")?;

    Ok(RunRecord {
        run_id: column(row, "run_id")?,
        flow_name: column(row, "flow_name")?,
        flow: column(row, "flow")?,
        status: status_str.parse::<RunStatus>()?,
        started_at: column(row, "started_at")?,
        finished_at: column(row, "finished_at")?,
        duration_ms: duration_ms as u64,
        task_results: decode_stored_json(&task_results_str, "runs.task_results")?,
        agent_count: agent_count as usize,
        task_count: task_count as usize,
        total_tokens: total_tokens as u32,
        cached_tokens: cached_tokens as u32,
        tags: decode_stored_json(&tags_str, "runs.tags")?,
        owner_instance_id: column(row, "owner_instance_id")?,
        lease_expires_at: column(row, "lease_expires_at")?,
    })
}

/// Convert a row from the summary query into a RunSummary (no task_results).
pub(in crate::engine::postgres_store) fn run_summary(row: &PgRow) -> Result<RunSummary> {
    let status_str: String = column(row, "status")?;
    let tags_str: String = column(row, "tags")?;
    let duration_ms: i64 = column(row, "duration_ms")?;
    let agent_count: i32 = column(row, "agent_count")?;
    let task_count: i32 = column(row, "task_count")?;
    let total_tokens: i32 = column(row, "total_tokens")?;
    let cached_tokens: i32 = column(row, "cached_tokens")?;

    Ok(RunSummary {
        run_id: column(row, "run_id")?,
        flow_name: column(row, "flow_name")?,
        flow: column(row, "flow")?,
        status: status_str.parse::<RunStatus>()?,
        started_at: column(row, "started_at")?,
        finished_at: column(row, "finished_at")?,
        duration_ms: duration_ms as u64,
        agent_count: agent_count as usize,
        task_count: task_count as usize,
        total_tokens: total_tokens as u32,
        cached_tokens: cached_tokens as u32,
        tags: decode_stored_json(&tags_str, "runs.tags")?,
    })
}

fn column<'r, T>(row: &'r PgRow, name: &str) -> Result<T>
where
    T: sqlx::Decode<'r, sqlx::Postgres> + sqlx::Type<sqlx::Postgres>,
{
    row.try_get(name)
        .map_err(|error| IronCrewError::Validation(format!("Column error: {error}")))
}
