use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::engine::task::TaskResult;
use crate::utils::error::{IronCrewError, Result};

use super::store::StateStore;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunRecord {
    pub run_id: String,
    pub flow_name: String,
    pub status: RunStatus,
    pub started_at: String,
    pub finished_at: String,
    pub duration_ms: u64,
    pub task_results: Vec<TaskResult>,
    pub agent_count: usize,
    pub task_count: usize,
    #[serde(default)]
    pub total_tokens: u32,
    #[serde(default)]
    pub cached_tokens: u32,
    #[serde(default)]
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum RunStatus {
    Success,
    PartialFailure,
    Failed,
}

impl std::fmt::Display for RunStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RunStatus::Success => write!(f, "success"),
            RunStatus::PartialFailure => write!(f, "partial_failure"),
            RunStatus::Failed => write!(f, "failed"),
        }
    }
}

/// Lightweight run metadata — same as `RunRecord` without `task_results`.
/// Used for paginated list views so clients don't pay to transfer every
/// historical task output when they only want a summary table.
///
/// Use `get_run` to fetch the full `RunRecord` by ID.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunSummary {
    pub run_id: String,
    pub flow_name: String,
    pub status: RunStatus,
    pub started_at: String,
    pub finished_at: String,
    pub duration_ms: u64,
    pub agent_count: usize,
    pub task_count: usize,
    #[serde(default)]
    pub total_tokens: u32,
    #[serde(default)]
    pub cached_tokens: u32,
    #[serde(default)]
    pub tags: Vec<String>,
}

impl From<&RunRecord> for RunSummary {
    fn from(record: &RunRecord) -> Self {
        Self {
            run_id: record.run_id.clone(),
            flow_name: record.flow_name.clone(),
            status: record.status.clone(),
            started_at: record.started_at.clone(),
            finished_at: record.finished_at.clone(),
            duration_ms: record.duration_ms,
            agent_count: record.agent_count,
            task_count: record.task_count,
            total_tokens: record.total_tokens,
            cached_tokens: record.cached_tokens,
            tags: record.tags.clone(),
        }
    }
}

/// Filter criteria for listing runs. All fields are optional; `None` means
/// "don't filter on this dimension".
#[derive(Debug, Clone, Default)]
pub struct ListRunsFilter {
    /// Status filter — e.g. `"success"`, `"partial_failure"`, `"failed"`.
    pub status: Option<String>,
    /// Tag filter — matches runs that contain the given tag in their tags list.
    pub tag: Option<String>,
    /// Only return runs started at or after this RFC3339 timestamp.
    pub since: Option<String>,
}

/// Shared filter-check used by the JSON backend. Returns true if `record`
/// matches every non-None field of `filter`.
fn filter_matches(record: &RunRecord, filter: &ListRunsFilter) -> bool {
    if let Some(ref status) = filter.status
        && record.status.to_string() != *status
    {
        return false;
    }
    if let Some(ref tag) = filter.tag
        && !record.tags.iter().any(|t| t == tag)
    {
        return false;
    }
    if let Some(ref since) = filter.since
        && record.started_at.as_str() < since.as_str()
    {
        return false;
    }
    true
}

/// JSON file-based store for persisting run records to disk.
pub struct JsonFileStore {
    store_dir: PathBuf,
}

impl JsonFileStore {
    pub fn new(store_dir: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(&store_dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            // Restrict .ironcrew/runs/ to owner-only (run history may contain sensitive output)
            let _ = std::fs::set_permissions(&store_dir, std::fs::Permissions::from_mode(0o700));
        }
        Ok(Self { store_dir })
    }
}

#[async_trait]
impl StateStore for JsonFileStore {
    async fn save_run(&self, record: &RunRecord) -> Result<String> {
        let filename = format!("{}.json", record.run_id);
        let path = self.store_dir.join(&filename);
        let json = serde_json::to_string_pretty(record)
            .map_err(|e| IronCrewError::Validation(format!("Failed to serialize run: {}", e)))?;
        std::fs::write(&path, json)?;
        tracing::info!("Run saved: {} -> {}", record.run_id, path.display());
        Ok(record.run_id.clone())
    }

    async fn get_run(&self, run_id: &str) -> Result<RunRecord> {
        let filename = format!("{}.json", run_id);
        let path = self.store_dir.join(&filename);
        if !path.exists() {
            return Err(IronCrewError::Validation(format!(
                "Run '{}' not found",
                run_id
            )));
        }
        let data = std::fs::read_to_string(&path)?;
        let record: RunRecord = serde_json::from_str(&data)
            .map_err(|e| IronCrewError::Validation(format!("Failed to parse run: {}", e)))?;
        Ok(record)
    }

    async fn list_runs(&self, status_filter: Option<&str>) -> Result<Vec<RunRecord>> {
        let mut runs = Vec::new();
        for entry in std::fs::read_dir(&self.store_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("json") {
                let data = std::fs::read_to_string(&path)?;
                if let Ok(record) = serde_json::from_str::<RunRecord>(&data) {
                    if let Some(filter) = status_filter
                        && record.status.to_string() != filter
                    {
                        continue;
                    }
                    runs.push(record);
                }
            }
        }
        // Sort by started_at descending (newest first)
        runs.sort_by(|a, b| b.started_at.cmp(&a.started_at));
        Ok(runs)
    }

    async fn list_runs_summary(
        &self,
        filter: &ListRunsFilter,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<RunSummary>> {
        // For the JSON backend there's no cheaper way than reading each file,
        // but we can at least produce RunSummary and drop task_results from
        // memory as soon as possible. The winning optimization here would be
        // a sidecar index file — out of scope for this tier.
        let mut summaries = Vec::new();
        for entry in std::fs::read_dir(&self.store_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let data = std::fs::read_to_string(&path)?;
            let Ok(record) = serde_json::from_str::<RunRecord>(&data) else {
                continue;
            };
            if !filter_matches(&record, filter) {
                continue;
            }
            summaries.push(RunSummary::from(&record));
            // `record` is dropped here — task_results memory freed before next iteration
        }

        // Sort newest-first
        summaries.sort_by(|a, b| b.started_at.cmp(&a.started_at));

        // Apply offset and limit
        let start = offset.min(summaries.len());
        summaries.drain(..start);
        if limit > 0 && summaries.len() > limit {
            summaries.truncate(limit);
        }
        Ok(summaries)
    }

    async fn count_runs(&self, filter: &ListRunsFilter) -> Result<u64> {
        let mut count: u64 = 0;
        for entry in std::fs::read_dir(&self.store_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let data = std::fs::read_to_string(&path)?;
            if let Ok(record) = serde_json::from_str::<RunRecord>(&data)
                && filter_matches(&record, filter)
            {
                count += 1;
            }
        }
        Ok(count)
    }

    async fn delete_run(&self, run_id: &str) -> Result<()> {
        let filename = format!("{}.json", run_id);
        let path = self.store_dir.join(&filename);
        if !path.exists() {
            return Err(IronCrewError::Validation(format!(
                "Run '{}' not found",
                run_id
            )));
        }
        std::fs::remove_file(&path)?;
        Ok(())
    }
}

/// Backward-compatible alias for `JsonFileStore`.
#[allow(dead_code)]
pub type RunHistory = JsonFileStore;
