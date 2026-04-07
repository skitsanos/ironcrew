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

/// JSON file-based store for persisting run records to disk.
pub struct JsonFileStore {
    store_dir: PathBuf,
}

impl JsonFileStore {
    pub fn new(store_dir: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(&store_dir)?;
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
