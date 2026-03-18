use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::engine::task::TaskResult;
use crate::utils::error::{IronCrewError, Result};

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

/// Store for persisting run records to disk.
pub struct RunHistory {
    store_dir: PathBuf,
}

impl RunHistory {
    pub fn new(store_dir: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(&store_dir)?;
        Ok(Self { store_dir })
    }

    /// Save a run record. Returns the run_id.
    pub fn save(&self, record: &RunRecord) -> Result<String> {
        let filename = format!("{}.json", record.run_id);
        let path = self.store_dir.join(&filename);
        let json = serde_json::to_string_pretty(record)
            .map_err(|e| IronCrewError::Validation(format!("Failed to serialize run: {}", e)))?;
        std::fs::write(&path, json)?;
        tracing::info!("Run saved: {} -> {}", record.run_id, path.display());
        Ok(record.run_id.clone())
    }

    /// Load a run record by ID.
    pub fn get(&self, run_id: &str) -> Result<RunRecord> {
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

    /// List all runs, optionally filtered by status.
    pub fn list(&self, status_filter: Option<&str>) -> Result<Vec<RunRecord>> {
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

    /// Delete a run record.
    #[allow(dead_code)]
    pub fn delete(&self, run_id: &str) -> Result<()> {
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
