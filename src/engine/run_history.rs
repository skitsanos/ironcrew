use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::engine::sessions::{ConversationRecord, ConversationSummary, DialogStateRecord};
use crate::engine::task::TaskResult;
use crate::utils::error::{IronCrewError, Result};

use super::store::StateStore;

/// Sanitize an arbitrary flow label so it's safe to use as a filename
/// component. Keeps alphanumerics and `-_.`; replaces the rest with `_`.
/// The input flow_path is already validated at crate entry points, but
/// we defensively sanitize here too.
fn sanitize_flow_component(flow_path: &str) -> String {
    flow_path
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Resolve the on-disk JSON path for a conversation record. When
/// `flow_path` is `Some(..)`, the file is namespaced under
/// `<conversations_dir>/<flow>/<id>.json`. When `None`, falls back to
/// the legacy flat layout for backwards compatibility.
fn conversation_file_path(conversations_dir: &Path, flow_path: Option<&str>, id: &str) -> PathBuf {
    match flow_path {
        Some(flow) => {
            let flow_dir = conversations_dir.join(sanitize_flow_component(flow));
            let _ = std::fs::create_dir_all(&flow_dir);
            flow_dir.join(format!("{}.json", id))
        }
        None => conversations_dir.join(format!("{}.json", id)),
    }
}

/// Load and parse a conversation record file. Returns `Ok(None)` when
/// the file is missing.
fn load_conversation_file(path: &Path, id: &str) -> Result<Option<ConversationRecord>> {
    if !path.exists() {
        return Ok(None);
    }
    let data = std::fs::read_to_string(path)?;
    let record: ConversationRecord = serde_json::from_str(&data).map_err(|e| {
        IronCrewError::Validation(format!("Failed to parse conversation '{}': {}", id, e))
    })?;
    Ok(Some(record))
}

/// Walk every conversation JSON file under `conversations_dir`, invoking
/// `visit` for each record that matches the optional flow filter.
/// Handles both the legacy flat layout (`<id>.json`) and the current
/// scoped layout (`<flow>/<id>.json`).
fn walk_conversation_records(
    conversations_dir: &Path,
    flow_path: Option<&str>,
    visit: &mut dyn FnMut(ConversationRecord),
) -> Result<()> {
    if !conversations_dir.exists() {
        return Ok(());
    }
    for entry in std::fs::read_dir(conversations_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            // Scoped subdir: iterate its JSON files.
            for sub in std::fs::read_dir(&path)? {
                let sub = sub?;
                let sub_path = sub.path();
                if sub_path.extension().and_then(|e| e.to_str()) != Some("json") {
                    continue;
                }
                if let Some(record) = read_record_for_walk(&sub_path)
                    && flow_filter_matches(&record, flow_path)
                {
                    visit(record);
                }
            }
        } else if path.extension().and_then(|e| e.to_str()) == Some("json") {
            // Legacy flat record.
            if let Some(record) = read_record_for_walk(&path)
                && flow_filter_matches(&record, flow_path)
            {
                visit(record);
            }
        }
    }
    Ok(())
}

fn read_record_for_walk(path: &Path) -> Option<ConversationRecord> {
    let data = std::fs::read_to_string(path).ok()?;
    serde_json::from_str::<ConversationRecord>(&data).ok()
}

fn flow_filter_matches(record: &ConversationRecord, flow_path: Option<&str>) -> bool {
    match flow_path {
        Some(fp) => record.flow_path.as_deref() == Some(fp),
        None => true,
    }
}

// ── Dialog on-disk helpers (mirror the conversation helpers above) ──────

fn dialog_file_path(dialogs_dir: &Path, flow_path: Option<&str>, id: &str) -> PathBuf {
    match flow_path {
        Some(flow) => {
            let flow_dir = dialogs_dir.join(sanitize_flow_component(flow));
            let _ = std::fs::create_dir_all(&flow_dir);
            flow_dir.join(format!("{}.json", id))
        }
        None => dialogs_dir.join(format!("{}.json", id)),
    }
}

fn load_dialog_file(path: &Path, id: &str) -> Result<Option<DialogStateRecord>> {
    if !path.exists() {
        return Ok(None);
    }
    let data = std::fs::read_to_string(path)?;
    let record: DialogStateRecord = serde_json::from_str(&data).map_err(|e| {
        IronCrewError::Validation(format!("Failed to parse dialog state '{}': {}", id, e))
    })?;
    Ok(Some(record))
}

fn walk_dialog_records(
    dialogs_dir: &Path,
    flow_path: Option<&str>,
    visit: &mut dyn FnMut(DialogStateRecord),
) -> Result<()> {
    if !dialogs_dir.exists() {
        return Ok(());
    }
    for entry in std::fs::read_dir(dialogs_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            for sub in std::fs::read_dir(&path)? {
                let sub = sub?;
                let sub_path = sub.path();
                if sub_path.extension().and_then(|e| e.to_str()) != Some("json") {
                    continue;
                }
                if let Some(record) = read_dialog_for_walk(&sub_path)
                    && dialog_flow_matches(&record, flow_path)
                {
                    visit(record);
                }
            }
        } else if path.extension().and_then(|e| e.to_str()) == Some("json")
            && let Some(record) = read_dialog_for_walk(&path)
            && dialog_flow_matches(&record, flow_path)
        {
            visit(record);
        }
    }
    Ok(())
}

fn read_dialog_for_walk(path: &Path) -> Option<DialogStateRecord> {
    let data = std::fs::read_to_string(path).ok()?;
    serde_json::from_str::<DialogStateRecord>(&data).ok()
}

fn dialog_flow_matches(record: &DialogStateRecord, flow_path: Option<&str>) -> bool {
    match flow_path {
        Some(fp) => record.flow_path.as_deref() == Some(fp),
        None => true,
    }
}

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
    Running,
    Abandoned,
    Success,
    PartialFailure,
    Failed,
}

impl std::fmt::Display for RunStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RunStatus::Running => write!(f, "running"),
            RunStatus::Abandoned => write!(f, "abandoned"),
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

/// JSON file-based store rooted at an `.ironcrew/` directory.
///
/// Each record type gets its own subdirectory: `runs/`, `conversations/`,
/// and `dialogs/`. All three are owner-only (0o700) on Unix since they
/// may contain sensitive model output.
pub struct JsonFileStore {
    runs_dir: PathBuf,
    conversations_dir: PathBuf,
    dialogs_dir: PathBuf,
}

impl JsonFileStore {
    /// Create (or open) a JSON-backed store inside the given `.ironcrew/`
    /// directory. The directory — and the three subdirectories it contains
    /// — are created with `create_dir_all` if they don't already exist.
    pub fn new(ironcrew_dir: PathBuf) -> Result<Self> {
        let runs_dir = ironcrew_dir.join("runs");
        let conversations_dir = ironcrew_dir.join("conversations");
        let dialogs_dir = ironcrew_dir.join("dialogs");

        for dir in [&runs_dir, &conversations_dir, &dialogs_dir] {
            std::fs::create_dir_all(dir)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
            }
        }

        Ok(Self {
            runs_dir,
            conversations_dir,
            dialogs_dir,
        })
    }
}

#[async_trait]
impl StateStore for JsonFileStore {
    async fn save_run(&self, record: &RunRecord) -> Result<String> {
        let filename = format!("{}.json", record.run_id);
        let path = self.runs_dir.join(&filename);
        let json = serde_json::to_string_pretty(record)
            .map_err(|e| IronCrewError::Validation(format!("Failed to serialize run: {}", e)))?;
        std::fs::write(&path, json)?;
        tracing::info!("Run saved: {} -> {}", record.run_id, path.display());
        Ok(record.run_id.clone())
    }

    async fn save_run_intent(
        &self,
        _suggested_id: Option<String>,
        _flow_name: &str,
        _started_at: &str,
        _agent_count: usize,
        _task_count: usize,
        _tags: &[String],
    ) -> Result<String> {
        unimplemented!("save_run_intent — landed in Task 3")
    }

    async fn update_run_completion(
        &self,
        _run_id: &str,
        _status: RunStatus,
        _finished_at: &str,
        _duration_ms: u64,
        _task_results: Vec<crate::engine::task::TaskResult>,
        _total_tokens: u32,
        _cached_tokens: u32,
    ) -> Result<()> {
        unimplemented!("update_run_completion — landed in Task 3")
    }

    async fn reconcile_abandoned_runs(&self, _now: &str) -> Result<usize> {
        unimplemented!("reconcile_abandoned_runs — landed in Task 3")
    }

    async fn get_run(&self, run_id: &str) -> Result<RunRecord> {
        let filename = format!("{}.json", run_id);
        let path = self.runs_dir.join(&filename);
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
        for entry in std::fs::read_dir(&self.runs_dir)? {
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
        for entry in std::fs::read_dir(&self.runs_dir)? {
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
        let path = self.runs_dir.join(&filename);
        if !path.exists() {
            return Err(IronCrewError::Validation(format!(
                "Run '{}' not found",
                run_id
            )));
        }
        std::fs::remove_file(&path)?;
        Ok(())
    }

    // ─── Persistent sessions ────────────────────────────────────────────────
    //
    // `get_*` returns Ok(None) when the file is missing so the caller can
    // tell "first time this id is used" apart from real I/O errors.

    async fn save_conversation(&self, record: &ConversationRecord) -> Result<()> {
        // Scope the on-disk filename by flow to prevent two flows sharing
        // the same `id` from clobbering each other. Legacy records
        // (flow_path = None) keep the old `<id>.json` layout.
        let path = conversation_file_path(
            &self.conversations_dir,
            record.flow_path.as_deref(),
            &record.id,
        );
        let json = serde_json::to_string_pretty(record).map_err(|e| {
            IronCrewError::Validation(format!("Failed to serialize conversation: {}", e))
        })?;
        std::fs::write(&path, json)?;
        tracing::debug!("Conversation saved: {} -> {}", record.id, path.display());
        Ok(())
    }

    async fn get_conversation(
        &self,
        flow_path: Option<&str>,
        id: &str,
    ) -> Result<Option<ConversationRecord>> {
        match flow_path {
            Some(requested) => {
                // Scoped read — only look at the flow's own subdirectory.
                // Defence-in-depth: even if the file exists, verify the
                // record's own flow_path matches.
                let path = conversation_file_path(&self.conversations_dir, Some(requested), id);
                let Some(record) = load_conversation_file(&path, id)? else {
                    return Ok(None);
                };
                if record.flow_path.as_deref() != Some(requested) {
                    return Ok(None);
                }
                Ok(Some(record))
            }
            None => {
                // Global/admin lookup — search every flow subdirectory
                // plus the legacy flat layout. Returns the first match.
                let mut found: Option<ConversationRecord> = None;
                walk_conversation_records(&self.conversations_dir, None, &mut |r| {
                    if found.is_none() && r.id == id {
                        found = Some(r);
                    }
                })?;
                Ok(found)
            }
        }
    }

    async fn delete_conversation(&self, flow_path: Option<&str>, id: &str) -> Result<()> {
        let path = conversation_file_path(&self.conversations_dir, flow_path, id);
        if !path.exists() {
            return Ok(());
        }
        // Defence in depth: refuse to delete if the record's flow_path
        // disagrees with the requested scope.
        if let Some(requested) = flow_path {
            let record = load_conversation_file(&path, id)?;
            if let Some(r) = record
                && r.flow_path.as_deref() != Some(requested)
            {
                return Ok(());
            }
        }
        std::fs::remove_file(&path)?;
        Ok(())
    }

    async fn list_conversations(
        &self,
        flow_path: Option<&str>,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<ConversationSummary>> {
        let mut summaries: Vec<ConversationSummary> = Vec::new();
        walk_conversation_records(&self.conversations_dir, flow_path, &mut |record| {
            summaries.push(ConversationSummary::from(&record));
        })?;
        summaries.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));

        let start = offset.min(summaries.len());
        summaries.drain(..start);
        if limit > 0 && summaries.len() > limit {
            summaries.truncate(limit);
        }
        Ok(summaries)
    }

    async fn count_conversations(&self, flow_path: Option<&str>) -> Result<u64> {
        let mut count: u64 = 0;
        walk_conversation_records(&self.conversations_dir, flow_path, &mut |_| {
            count += 1;
        })?;
        Ok(count)
    }

    async fn save_dialog_state(&self, record: &DialogStateRecord) -> Result<()> {
        // Scope on-disk filename by flow — mirrors the conversation layout.
        // Legacy records with `flow_path = None` stay at `<id>.json`.
        let path = dialog_file_path(&self.dialogs_dir, record.flow_path.as_deref(), &record.id);
        let json = serde_json::to_string_pretty(record).map_err(|e| {
            IronCrewError::Validation(format!("Failed to serialize dialog state: {}", e))
        })?;
        std::fs::write(&path, json)?;
        tracing::debug!("Dialog state saved: {} -> {}", record.id, path.display());
        Ok(())
    }

    async fn get_dialog_state(
        &self,
        flow_path: Option<&str>,
        id: &str,
    ) -> Result<Option<DialogStateRecord>> {
        match flow_path {
            Some(requested) => {
                let path = dialog_file_path(&self.dialogs_dir, Some(requested), id);
                let Some(record) = load_dialog_file(&path, id)? else {
                    return Ok(None);
                };
                if record.flow_path.as_deref() != Some(requested) {
                    return Ok(None);
                }
                Ok(Some(record))
            }
            None => {
                // Global/admin lookup — search every flow subdirectory plus
                // the legacy flat `<id>.json` layout for backwards compat.
                let mut found: Option<DialogStateRecord> = None;
                walk_dialog_records(&self.dialogs_dir, None, &mut |r| {
                    if found.is_none() && r.id == id {
                        found = Some(r);
                    }
                })?;
                Ok(found)
            }
        }
    }

    async fn delete_dialog_state(&self, flow_path: Option<&str>, id: &str) -> Result<()> {
        let path = dialog_file_path(&self.dialogs_dir, flow_path, id);
        if !path.exists() {
            return Ok(());
        }
        if let Some(requested) = flow_path {
            let record = load_dialog_file(&path, id)?;
            if let Some(r) = record
                && r.flow_path.as_deref() != Some(requested)
            {
                return Ok(());
            }
        }
        std::fs::remove_file(&path)?;
        Ok(())
    }
}
