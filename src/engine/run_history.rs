use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use crate::engine::sessions::{
    ConversationRecord, ConversationSummary, DialogStateRecord, validate_session_id,
};
use crate::engine::task::TaskResult;
use crate::utils::error::{IronCrewError, Result};

use super::store::StateStore;

type JsonRunLockRegistry = Mutex<std::collections::HashMap<PathBuf, Weak<Mutex<()>>>>;
static JSON_RUN_LOCKS: OnceLock<JsonRunLockRegistry> = OnceLock::new();

const DEFAULT_JSON_RECORD_MAX_BYTES: usize = 64 * 1024 * 1024;
const HARD_JSON_RECORD_MAX_BYTES: usize = 128 * 1024 * 1024;
const DEFAULT_JSON_SCAN_MAX_ENTRIES: usize = 10_000;
const HARD_JSON_SCAN_MAX_ENTRIES: usize = 100_000;

fn json_record_max_bytes() -> usize {
    match std::env::var("IRONCREW_JSON_STORE_RECORD_MAX_BYTES") {
        Ok(raw) => raw
            .parse::<usize>()
            .ok()
            .filter(|value| *value > 0)
            .map(|value| value.min(HARD_JSON_RECORD_MAX_BYTES))
            .unwrap_or(DEFAULT_JSON_RECORD_MAX_BYTES),
        Err(_) => DEFAULT_JSON_RECORD_MAX_BYTES,
    }
}

fn json_scan_max_entries() -> usize {
    match std::env::var("IRONCREW_JSON_STORE_MAX_SCAN_ENTRIES") {
        Ok(raw) => raw
            .parse::<usize>()
            .ok()
            .filter(|value| *value > 0)
            .map(|value| value.min(HARD_JSON_SCAN_MAX_ENTRIES))
            .unwrap_or(DEFAULT_JSON_SCAN_MAX_ENTRIES),
        Err(_) => DEFAULT_JSON_SCAN_MAX_ENTRIES,
    }
}

fn consume_scan_entry(scanned: &mut usize) -> Result<()> {
    *scanned = scanned.saturating_add(1);
    let limit = json_scan_max_entries();
    if *scanned > limit {
        return Err(IronCrewError::Validation(format!(
            "JSON store scan exceeds IRONCREW_JSON_STORE_MAX_SCAN_ENTRIES ({limit}); use PostgreSQL for larger stores"
        )));
    }
    Ok(())
}

fn read_json_record(path: &Path) -> Result<String> {
    let mut file = std::fs::File::open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(IronCrewError::Validation(format!(
            "JSON store path '{}' is not a regular file",
            path.display()
        )));
    }
    let max_bytes = json_record_max_bytes();
    if metadata.len() > max_bytes as u64 {
        return Err(IronCrewError::Validation(format!(
            "JSON store record '{}' is {} bytes, exceeds IRONCREW_JSON_STORE_RECORD_MAX_BYTES ({max_bytes})",
            path.display(),
            metadata.len()
        )));
    }
    let mut bytes = Vec::with_capacity((metadata.len() as usize).min(max_bytes));
    std::io::Read::by_ref(&mut file)
        .take(max_bytes as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > max_bytes {
        return Err(IronCrewError::Validation(format!(
            "JSON store record '{}' grew beyond IRONCREW_JSON_STORE_RECORD_MAX_BYTES ({max_bytes}) while reading",
            path.display()
        )));
    }
    String::from_utf8(bytes).map_err(|error| {
        IronCrewError::Validation(format!(
            "JSON store record '{}' is not valid UTF-8: {error}",
            path.display()
        ))
    })
}

struct BoundedJsonBuffer {
    bytes: Vec<u8>,
    max_bytes: usize,
}

impl Write for BoundedJsonBuffer {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        if self.bytes.len().saturating_add(buffer.len()) > self.max_bytes {
            return Err(std::io::Error::new(
                std::io::ErrorKind::FileTooLarge,
                format!("serialized JSON exceeds {} bytes", self.max_bytes),
            ));
        }
        self.bytes.extend_from_slice(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn serialize_json_record<T: Serialize>(value: &T, label: &str) -> Result<Vec<u8>> {
    let max_bytes = json_record_max_bytes();
    let mut output = BoundedJsonBuffer {
        bytes: Vec::with_capacity(max_bytes.min(64 * 1024)),
        max_bytes,
    };
    serde_json::to_writer_pretty(&mut output, value).map_err(|error| {
        IronCrewError::Validation(format!(
            "Failed to serialize {label} within IRONCREW_JSON_STORE_RECORD_MAX_BYTES ({max_bytes}): {error}"
        ))
    })?;
    Ok(output.bytes)
}

fn write_serialized_record_atomic<T: Serialize>(path: &Path, value: &T, label: &str) -> Result<()> {
    let bytes = serialize_json_record(value, label)?;
    let parent = path.parent().ok_or_else(|| {
        IronCrewError::Validation(format!(
            "JSON record path '{}' has no parent",
            path.display()
        ))
    })?;
    let temporary = parent.join(format!(
        ".{}.{}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("record"),
        uuid::Uuid::new_v4()
    ));
    let result = (|| -> Result<()> {
        let mut options = std::fs::OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temporary, path)?;
        std::fs::File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

fn write_serialized_record_create_new<T: Serialize>(
    path: &Path,
    value: &T,
    label: &str,
) -> Result<()> {
    let bytes = serialize_json_record(value, label)?;
    let parent = path.parent().ok_or_else(|| {
        IronCrewError::Validation(format!(
            "JSON record path '{}' has no parent",
            path.display()
        ))
    })?;
    let temporary = parent.join(format!(
        ".{}.{}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("record"),
        uuid::Uuid::new_v4()
    ));
    let result = (|| -> Result<()> {
        let mut options = std::fs::OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        drop(file);
        // Same-directory hard-link publication is atomic and never replaces
        // an existing run id. The temporary name is removed after the link is
        // durable; both names refer to the same fully-written inode meanwhile.
        std::fs::hard_link(&temporary, path)?;
        std::fs::remove_file(&temporary)?;
        std::fs::File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

/// Store handles are constructed independently by the CLI heartbeat and Lua
/// runtime. Share one process-local lock per runs directory so their
/// read-modify-write cycles cannot resurrect a record that another handle just
/// terminalized.
fn shared_json_run_lock(runs_dir: &Path) -> Arc<Mutex<()>> {
    let key = std::fs::canonicalize(runs_dir).unwrap_or_else(|_| runs_dir.to_path_buf());
    let registry = JSON_RUN_LOCKS.get_or_init(|| Mutex::new(std::collections::HashMap::new()));
    let mut registry = registry
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    registry.retain(|_, lock| lock.strong_count() > 0);
    if let Some(lock) = registry.get(&key).and_then(Weak::upgrade) {
        return lock;
    }
    let lock = Arc::new(Mutex::new(()));
    registry.insert(key, Arc::downgrade(&lock));
    lock
}

/// Encode a flow label into one collision-resistant filename component.
///
/// Ordinary ASCII flow names retain their pre-hardening on-disk layout. Every
/// other UTF-8 byte is percent encoded, including `%`, so names such as `a+b`
/// and `a?b` can no longer collapse into the same JSON-store directory. Very
/// long encodings use a reserved, content-addressed form to remain below
/// filesystem component limits; a literal leading `%` is itself encoded and
/// therefore cannot collide with that namespace.
fn encode_flow_component(flow_path: &str) -> String {
    use std::fmt::Write as _;

    if flow_path.is_empty() {
        return "%E".to_string();
    }
    if matches!(flow_path, "." | "..") {
        return flow_path.replace('.', "%2E");
    }

    let mut encoded = String::with_capacity(flow_path.len());
    for byte in flow_path.as_bytes() {
        if byte.is_ascii_alphanumeric() || matches!(*byte, b'-' | b'_' | b'.') {
            encoded.push(char::from(*byte));
        } else {
            write!(&mut encoded, "%{byte:02X}").expect("writing to String cannot fail");
        }
    }

    if encoded.len() <= 240 {
        encoded
    } else {
        use sha2::{Digest, Sha256};
        let digest = Sha256::digest(flow_path.as_bytes());
        let mut hashed = String::with_capacity(66);
        hashed.push_str("%H");
        for byte in digest {
            write!(&mut hashed, "{byte:02x}").expect("writing to String cannot fail");
        }
        hashed
    }
}

fn validate_run_id(run_id: &str) -> Result<()> {
    if run_id.is_empty() || run_id.len() > 128 {
        return Err(IronCrewError::Validation(
            "run id must contain between 1 and 128 ASCII characters".into(),
        ));
    }
    if !run_id
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(IronCrewError::Validation(
            "run id contains an invalid character".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod path_component_tests {
    use super::{encode_flow_component, validate_run_id};

    #[test]
    fn ordinary_flow_names_keep_their_existing_component() {
        assert_eq!(encode_flow_component("chat-http.v2"), "chat-http.v2");
    }

    #[test]
    fn unusual_flow_names_do_not_collapse_or_escape() {
        let plus = encode_flow_component("a+b");
        let question = encode_flow_component("a?b");
        assert_ne!(plus, question);
        assert_eq!(plus, "a%2Bb");
        assert_eq!(question, "a%3Fb");
        assert_eq!(encode_flow_component("."), "%2E");
        assert_eq!(encode_flow_component(".."), "%2E%2E");
        assert_eq!(encode_flow_component(""), "%E");
    }

    #[test]
    fn run_ids_are_safe_filename_components() {
        assert!(validate_run_id("550e8400-e29b-41d4-a716-446655440000").is_ok());
        assert!(validate_run_id("../outside").is_err());
        assert!(validate_run_id("").is_err());
    }
}

/// Resolve the on-disk JSON path for a conversation record. When
/// `flow_path` is `Some(..)`, the file is namespaced under
/// `<conversations_dir>/<flow>/<id>.json`. When `None`, falls back to
/// the legacy flat layout for backwards compatibility.
fn conversation_file_path(conversations_dir: &Path, flow_path: Option<&str>, id: &str) -> PathBuf {
    match flow_path {
        Some(flow) => {
            let flow_dir = conversations_dir.join(encode_flow_component(flow));
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
    let data = read_json_record(path)?;
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
    let mut scanned = 0usize;
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
                consume_scan_entry(&mut scanned)?;
                if let Some(record) = read_record_for_walk(&sub_path)
                    && flow_filter_matches(&record, flow_path)
                {
                    visit(record);
                }
            }
        } else if path.extension().and_then(|e| e.to_str()) == Some("json") {
            // Legacy flat record.
            consume_scan_entry(&mut scanned)?;
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
    let data = read_json_record(path).ok()?;
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
            let flow_dir = dialogs_dir.join(encode_flow_component(flow));
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
    let data = read_json_record(path)?;
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
    let mut scanned = 0usize;
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
                consume_scan_entry(&mut scanned)?;
                if let Some(record) = read_dialog_for_walk(&sub_path)
                    && dialog_flow_matches(&record, flow_path)
                {
                    visit(record);
                }
            }
        } else if path.extension().and_then(|e| e.to_str()) == Some("json") {
            consume_scan_entry(&mut scanned)?;
            if let Some(record) = read_dialog_for_walk(&path)
                && dialog_flow_matches(&record, flow_path)
            {
                visit(record);
            }
        }
    }
    Ok(())
}

fn read_dialog_for_walk(path: &Path) -> Option<DialogStateRecord> {
    let data = read_json_record(path).ok()?;
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
    /// The crew's `goal` text (human-readable). NOT a stable flow identifier —
    /// use `flow` for scoping.
    pub flow_name: String,
    /// Flow slug (the `{flow}` URL segment / project directory name) the run
    /// was launched under. Used to scope run endpoints so one flow cannot read
    /// or delete another's runs. Empty for pre-migration records and for CLI
    /// runs that aren't launched under a named flow directory.
    #[serde(default)]
    pub flow: String,
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
    /// Runtime instance that currently owns this in-flight run. Empty for
    /// records written before leases were introduced.
    #[serde(default)]
    pub owner_instance_id: String,
    /// RFC3339 deadline renewed by the owner heartbeat. Empty for legacy
    /// records, which the reconciler treats as unleased and therefore stale.
    #[serde(default)]
    pub lease_expires_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum RunStatus {
    Running,
    /// Suspended on `crew:ask_human()` — the flow coroutine is parked until a
    /// human answers or the question times out. In-flight like `Running`: the
    /// startup reconciler treats both as orphaned after a crash.
    WaitingForInput,
    Abandoned,
    Aborted,
    TimedOut,
    Success,
    PartialFailure,
    Failed,
}

impl std::fmt::Display for RunStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RunStatus::Running => write!(f, "running"),
            RunStatus::WaitingForInput => write!(f, "waiting_for_input"),
            RunStatus::Abandoned => write!(f, "abandoned"),
            RunStatus::Aborted => write!(f, "aborted"),
            RunStatus::TimedOut => write!(f, "timed_out"),
            RunStatus::Success => write!(f, "success"),
            RunStatus::PartialFailure => write!(f, "partial_failure"),
            RunStatus::Failed => write!(f, "failed"),
        }
    }
}

impl std::str::FromStr for RunStatus {
    type Err = crate::utils::error::IronCrewError;

    /// Inverse of `Display` — decodes the stored status string. Unknown values
    /// are a data-integrity error rather than a silent default, so a corrupt
    /// row surfaces instead of masquerading as a valid state. Single source of
    /// truth shared by the SQLite and Postgres backends.
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "running" => Ok(RunStatus::Running),
            "waiting_for_input" => Ok(RunStatus::WaitingForInput),
            "abandoned" => Ok(RunStatus::Abandoned),
            "aborted" => Ok(RunStatus::Aborted),
            "timed_out" => Ok(RunStatus::TimedOut),
            "success" => Ok(RunStatus::Success),
            "partial_failure" => Ok(RunStatus::PartialFailure),
            "failed" => Ok(RunStatus::Failed),
            other => Err(crate::utils::error::IronCrewError::Validation(format!(
                "Unknown run status '{}' in stored record",
                other
            ))),
        }
    }
}

impl RunStatus {
    pub fn is_in_flight(&self) -> bool {
        matches!(self, Self::Running | Self::WaitingForInput)
    }

    pub fn is_terminal(&self) -> bool {
        !self.is_in_flight()
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
    /// Flow slug the run was launched under (see `RunRecord::flow`).
    #[serde(default)]
    pub flow: String,
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
            flow: record.flow.clone(),
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
    /// Flow-slug scope — when `Some`, only runs launched under this flow are
    /// returned. Set by the HTTP layer from the `{flow}` URL segment so a
    /// flow's run list can't leak another flow's runs.
    pub flow: Option<String>,
    /// Status filter — e.g. `"success"`, `"partial_failure"`, `"failed"`.
    pub status: Option<String>,
    /// Tag filter — matches runs that contain the given tag in their tags list.
    pub tag: Option<String>,
    /// Only return runs started at or after this RFC3339 timestamp.
    pub since: Option<String>,
}

/// Fields written when a run starts (`save_run_intent`). Passed as one value
/// instead of a long positional argument list so call sites read as named
/// fields and the set can grow without reshuffling every caller.
#[derive(Debug, Clone, Default)]
pub struct RunIntent {
    /// Pre-chosen run id (e.g. allocated by the HTTP handler so SSE subscribers
    /// can join mid-flight). `None` lets the store generate a UUID.
    pub suggested_id: Option<String>,
    /// Human-readable crew goal.
    pub flow_name: String,
    /// Flow slug the run is scoped to (see [`RunRecord::flow`]).
    pub flow: String,
    pub started_at: String,
    pub agent_count: usize,
    pub task_count: usize,
    pub tags: Vec<String>,
}

/// Fields written when a run finishes (`update_run_completion`), transitioning a
/// `Running` record to a terminal state. The run id stays a separate argument
/// since it's the key, not part of the payload.
#[derive(Debug, Clone)]
pub struct RunCompletion {
    pub status: RunStatus,
    pub finished_at: String,
    pub duration_ms: u64,
    pub task_results: Vec<TaskResult>,
    pub total_tokens: u32,
    pub cached_tokens: u32,
}

/// Result of an atomic terminal transition. A second finalizer can observe
/// the winner without rewriting its status or payload.
#[derive(Debug, Clone, PartialEq)]
pub enum RunTransition {
    Applied,
    AlreadyTerminal(RunStatus),
}

impl RunCompletion {
    pub(crate) fn validate(&self) -> Result<()> {
        if !self.status.is_terminal() {
            return Err(IronCrewError::Validation(format!(
                "Run completion status must be terminal, got '{}'",
                self.status
            )));
        }
        Ok(())
    }
}

fn write_run_record_atomic(path: &Path, record: &RunRecord) -> Result<()> {
    write_serialized_record_atomic(path, record, "run record")
}

fn lease_is_expired(lease_expires_at: &str, now: chrono::DateTime<chrono::Utc>) -> bool {
    if lease_expires_at.is_empty() {
        return true;
    }
    chrono::DateTime::parse_from_rfc3339(lease_expires_at)
        .map(|deadline| deadline <= now)
        // Invalid legacy/corrupt leases must not make an in-flight row immortal.
        .unwrap_or(true)
}

/// Shared filter-check used by the JSON backend. Returns true if `record`
/// matches every non-None field of `filter`.
fn filter_matches(record: &RunRecord, filter: &ListRunsFilter) -> bool {
    if let Some(ref flow) = filter.flow
        && record.flow != *flow
    {
        return false;
    }
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
/// `dialogs/`, and `audit_events/`. All four are owner-only (0o700) on
/// Unix since they may contain sensitive model output.
pub struct JsonFileStore {
    runs_dir: PathBuf,
    conversations_dir: PathBuf,
    dialogs_dir: PathBuf,
    audit_events_dir: PathBuf,
    lease: super::store::RunLeaseConfig,
    run_lock: Arc<Mutex<()>>,
}

impl JsonFileStore {
    /// Create (or open) a JSON-backed store inside the given `.ironcrew/`
    /// directory. The directory — and the four subdirectories it contains
    /// — are created with `create_dir_all` if they don't already exist.
    pub fn new(ironcrew_dir: PathBuf) -> Result<Self> {
        Self::new_with_lease_config(ironcrew_dir, super::store::RunLeaseConfig::from_env()?)
    }

    pub fn new_with_lease_config(
        ironcrew_dir: PathBuf,
        lease: super::store::RunLeaseConfig,
    ) -> Result<Self> {
        let runs_dir = ironcrew_dir.join("runs");
        let conversations_dir = ironcrew_dir.join("conversations");
        let dialogs_dir = ironcrew_dir.join("dialogs");
        let audit_events_dir = ironcrew_dir.join("audit_events");

        for dir in [
            &runs_dir,
            &conversations_dir,
            &dialogs_dir,
            &audit_events_dir,
        ] {
            std::fs::create_dir_all(dir)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
            }
        }

        let run_lock = shared_json_run_lock(&runs_dir);
        Ok(Self {
            runs_dir,
            conversations_dir,
            dialogs_dir,
            audit_events_dir,
            lease,
            run_lock,
        })
    }
}

#[async_trait]
impl StateStore for JsonFileStore {
    async fn save_run_intent(&self, intent: RunIntent) -> Result<String> {
        if let Some(run_id) = intent.suggested_id.as_deref() {
            validate_run_id(run_id)?;
        }
        let _guard = self
            .run_lock
            .lock()
            .map_err(|e| IronCrewError::Validation(format!("JSON run lock poisoned: {}", e)))?;
        let run_id = intent
            .suggested_id
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let record = RunRecord {
            run_id: run_id.clone(),
            flow_name: intent.flow_name,
            flow: intent.flow,
            status: RunStatus::Running,
            started_at: intent.started_at,
            finished_at: String::new(),
            duration_ms: 0,
            task_results: Vec::new(),
            agent_count: intent.agent_count,
            task_count: intent.task_count,
            total_tokens: 0,
            cached_tokens: 0,
            tags: intent.tags,
            owner_instance_id: self.lease.instance_id().to_string(),
            lease_expires_at: self.lease.deadline_now(),
        };
        let filename = format!("{}.json", record.run_id);
        let path = self.runs_dir.join(&filename);
        write_serialized_record_create_new(&path, &record, "run intent").map_err(|error| {
            if matches!(&error, IronCrewError::Io(io) if io.kind() == std::io::ErrorKind::AlreadyExists)
            {
                IronCrewError::Validation(format!("Run '{}' already exists", run_id))
            } else {
                error
            }
        })?;
        tracing::debug!("Run intent saved: {} -> {}", run_id, path.display());
        Ok(run_id)
    }

    async fn update_run_completion(
        &self,
        run_id: &str,
        completion: RunCompletion,
    ) -> Result<RunTransition> {
        validate_run_id(run_id)?;
        completion.validate()?;
        let _guard = self
            .run_lock
            .lock()
            .map_err(|e| IronCrewError::Validation(format!("JSON run lock poisoned: {}", e)))?;
        let filename = format!("{}.json", run_id);
        let path = self.runs_dir.join(&filename);
        if !path.exists() {
            return Err(IronCrewError::Validation(format!(
                "Run '{}' not found (update_run_completion)",
                run_id
            )));
        }
        let data = read_json_record(&path)?;
        let mut record: RunRecord = serde_json::from_str(&data)
            .map_err(|e| IronCrewError::Validation(format!("Failed to parse run: {}", e)))?;
        if record.status.is_terminal() {
            return Ok(RunTransition::AlreadyTerminal(record.status));
        }
        if record.owner_instance_id != self.lease.instance_id() {
            return Err(IronCrewError::Validation(format!(
                "Run '{}' is owned by instance '{}', not '{}'",
                run_id,
                record.owner_instance_id,
                self.lease.instance_id()
            )));
        }
        record.status = completion.status;
        record.finished_at = completion.finished_at;
        record.duration_ms = completion.duration_ms;
        record.task_results = completion.task_results;
        record.total_tokens = completion.total_tokens;
        record.cached_tokens = completion.cached_tokens;
        record.lease_expires_at.clear();
        write_run_record_atomic(&path, &record)?;
        tracing::info!("Run completion saved: {} ({})", run_id, record.status);
        Ok(RunTransition::Applied)
    }

    async fn update_run_status(&self, run_id: &str, status: RunStatus) -> Result<()> {
        validate_run_id(run_id)?;
        if !status.is_in_flight() {
            return Err(IronCrewError::Validation(format!(
                "update_run_status requires an in-flight status, got '{}'",
                status
            )));
        }
        let _guard = self
            .run_lock
            .lock()
            .map_err(|e| IronCrewError::Validation(format!("JSON run lock poisoned: {}", e)))?;
        let filename = format!("{}.json", run_id);
        let path = self.runs_dir.join(&filename);
        if !path.exists() {
            return Err(IronCrewError::Validation(format!(
                "Run '{}' not found (update_run_status)",
                run_id
            )));
        }
        let data = read_json_record(&path)?;
        let mut record: RunRecord = serde_json::from_str(&data)
            .map_err(|e| IronCrewError::Validation(format!("Failed to parse run: {}", e)))?;
        if !record.status.is_in_flight() {
            return Err(IronCrewError::Validation(format!(
                "Run '{}' is not in an in-flight state (status={})",
                run_id, record.status
            )));
        }
        if record.owner_instance_id != self.lease.instance_id() {
            return Err(IronCrewError::Validation(format!(
                "Run '{}' is owned by instance '{}', not '{}'",
                run_id,
                record.owner_instance_id,
                self.lease.instance_id()
            )));
        }
        record.status = status;
        write_run_record_atomic(&path, &record)?;
        Ok(())
    }

    fn instance_id(&self) -> &str {
        self.lease.instance_id()
    }

    fn run_lease_ttl(&self) -> std::time::Duration {
        self.lease.ttl()
    }

    async fn heartbeat_owned_runs(&self) -> Result<usize> {
        let _guard = self
            .run_lock
            .lock()
            .map_err(|e| IronCrewError::Validation(format!("JSON run lock poisoned: {}", e)))?;
        let deadline = self.lease.deadline_now();
        let mut count = 0;
        let mut scanned = 0usize;
        for entry in std::fs::read_dir(&self.runs_dir)? {
            let path = entry?.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            consume_scan_entry(&mut scanned)?;
            let data = read_json_record(&path)?;
            let Ok(mut record) = serde_json::from_str::<RunRecord>(&data) else {
                continue;
            };
            if !record.status.is_in_flight() || record.owner_instance_id != self.lease.instance_id()
            {
                continue;
            }
            record.lease_expires_at.clone_from(&deadline);
            write_run_record_atomic(&path, &record)?;
            count += 1;
        }
        Ok(count)
    }

    async fn health_check(&self) -> Result<()> {
        let path = self
            .runs_dir
            .join(format!(".health-{}.tmp", uuid::Uuid::new_v4()));
        let result = (|| -> Result<()> {
            let mut file = std::fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&path)?;
            file.write_all(b"ok")?;
            drop(file);
            std::fs::remove_file(&path)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&path);
        }
        result
    }

    async fn reconcile_abandoned_runs(&self, now: &str) -> Result<usize> {
        let now_parsed = chrono::DateTime::parse_from_rfc3339(now)
            .map_err(|e| {
                IronCrewError::Validation(format!("Invalid reconciliation timestamp: {}", e))
            })?
            .with_timezone(&chrono::Utc);
        let _guard = self
            .run_lock
            .lock()
            .map_err(|e| IronCrewError::Validation(format!("JSON run lock poisoned: {}", e)))?;
        let mut count: usize = 0;
        let mut scanned = 0usize;
        for entry in std::fs::read_dir(&self.runs_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            consume_scan_entry(&mut scanned)?;
            let data = read_json_record(&path)?;
            let Ok(mut record) = serde_json::from_str::<RunRecord>(&data) else {
                continue;
            };
            if !record.status.is_in_flight()
                || !lease_is_expired(&record.lease_expires_at, now_parsed)
            {
                continue;
            }
            record.status = RunStatus::Abandoned;
            record.finished_at = now.to_string();
            record.lease_expires_at.clear();
            write_run_record_atomic(&path, &record)?;
            count += 1;
        }
        Ok(count)
    }

    async fn get_run(&self, run_id: &str) -> Result<RunRecord> {
        validate_run_id(run_id)?;
        let filename = format!("{}.json", run_id);
        let path = self.runs_dir.join(&filename);
        if !path.exists() {
            return Err(IronCrewError::Validation(format!(
                "Run '{}' not found",
                run_id
            )));
        }
        let data = read_json_record(&path)?;
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
        let mut scanned = 0usize;
        for entry in std::fs::read_dir(&self.runs_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            consume_scan_entry(&mut scanned)?;
            let data = read_json_record(&path)?;
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
        let mut scanned = 0usize;
        for entry in std::fs::read_dir(&self.runs_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            consume_scan_entry(&mut scanned)?;
            let data = read_json_record(&path)?;
            if let Ok(record) = serde_json::from_str::<RunRecord>(&data)
                && filter_matches(&record, filter)
            {
                count += 1;
            }
        }
        Ok(count)
    }

    async fn delete_run(&self, run_id: &str) -> Result<()> {
        validate_run_id(run_id)?;
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

    async fn save_conversation(&self, record: &ConversationRecord) -> Result<u64> {
        validate_session_id(&record.id)?;
        let _guard = self.run_lock.lock().map_err(|error| {
            IronCrewError::Validation(format!("JSON store lock error: {error}"))
        })?;
        // Scope the on-disk filename by flow to prevent two flows sharing
        // the same `id` from clobbering each other. Legacy records
        // (flow_path = None) keep the old `<id>.json` layout.
        let path = conversation_file_path(
            &self.conversations_dir,
            record.flow_path.as_deref(),
            &record.id,
        );
        let current = load_conversation_file(&path, &record.id)?;
        let current_revision = current.as_ref().map(|saved| saved.revision).unwrap_or(0);
        if current.is_some() && current_revision != record.revision
            || current.is_none() && record.revision != 0
        {
            return Err(IronCrewError::Conflict(format!(
                "Conversation '{}' changed since revision {}; reopen it before saving",
                record.id, record.revision
            )));
        }
        let next_revision = record
            .revision
            .checked_add(1)
            .ok_or_else(|| IronCrewError::Validation("Conversation revision overflow".into()))?;
        let mut to_write = record.clone();
        to_write.revision = next_revision;
        write_serialized_record_atomic(&path, &to_write, "conversation")?;
        tracing::debug!("Conversation saved: {} -> {}", record.id, path.display());
        Ok(next_revision)
    }

    async fn get_conversation(
        &self,
        flow_path: Option<&str>,
        id: &str,
    ) -> Result<Option<ConversationRecord>> {
        validate_session_id(id)?;
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
        validate_session_id(id)?;
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

    async fn save_dialog_state(&self, record: &DialogStateRecord) -> Result<u64> {
        validate_session_id(&record.id)?;
        let _guard = self.run_lock.lock().map_err(|error| {
            IronCrewError::Validation(format!("JSON store lock error: {error}"))
        })?;
        // Scope on-disk filename by flow — mirrors the conversation layout.
        // Legacy records with `flow_path = None` stay at `<id>.json`.
        let path = dialog_file_path(&self.dialogs_dir, record.flow_path.as_deref(), &record.id);
        let current = load_dialog_file(&path, &record.id)?;
        let current_revision = current.as_ref().map(|saved| saved.revision).unwrap_or(0);
        if current.is_some() && current_revision != record.revision
            || current.is_none() && record.revision != 0
        {
            return Err(IronCrewError::Conflict(format!(
                "Dialog '{}' changed since revision {}; reopen it before saving",
                record.id, record.revision
            )));
        }
        let next_revision = record
            .revision
            .checked_add(1)
            .ok_or_else(|| IronCrewError::Validation("Dialog revision overflow".into()))?;
        let mut to_write = record.clone();
        to_write.revision = next_revision;
        write_serialized_record_atomic(&path, &to_write, "dialog state")?;
        tracing::debug!("Dialog state saved: {} -> {}", record.id, path.display());
        Ok(next_revision)
    }

    async fn get_dialog_state(
        &self,
        flow_path: Option<&str>,
        id: &str,
    ) -> Result<Option<DialogStateRecord>> {
        validate_session_id(id)?;
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
        validate_session_id(id)?;
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

    async fn save_audit_event(&self, event: &crate::engine::audit::AuditEvent) -> Result<String> {
        let id = uuid::Uuid::new_v4().to_string();
        // Filename prefixed by timestamp (normalized — replace ':' with
        // '-' so a reverse-sorted directory listing is newest-first).
        let ts_safe = event.timestamp.replace(':', "-");
        let filename = format!("{}-{}.json", ts_safe, id);
        let path = self.audit_events_dir.join(&filename);

        let mut to_write = event.clone();
        to_write.id = id.clone();

        write_serialized_record_atomic(&path, &to_write, "audit event")?;
        tracing::debug!("Audit event saved: {} -> {}", id, path.display());
        Ok(id)
    }

    async fn list_audit_events(
        &self,
        filter: &crate::engine::audit::AuditFilter,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<crate::engine::audit::AuditEvent>> {
        let mut events: Vec<crate::engine::audit::AuditEvent> = Vec::new();
        let mut scanned = 0usize;
        for entry in std::fs::read_dir(&self.audit_events_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            consume_scan_entry(&mut scanned)?;
            let data = read_json_record(&path)?;
            let Ok(event) = serde_json::from_str::<crate::engine::audit::AuditEvent>(&data) else {
                continue;
            };
            if !filter.matches(&event) {
                continue;
            }
            events.push(event);
        }

        events.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));

        let start = offset.min(events.len());
        events.drain(..start);
        if limit > 0 && events.len() > limit {
            events.truncate(limit);
        }
        Ok(events)
    }

    async fn count_audit_events(&self, filter: &crate::engine::audit::AuditFilter) -> Result<u64> {
        let mut count: u64 = 0;
        let mut scanned = 0usize;
        for entry in std::fs::read_dir(&self.audit_events_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            consume_scan_entry(&mut scanned)?;
            let data = read_json_record(&path)?;
            if let Ok(event) = serde_json::from_str::<crate::engine::audit::AuditEvent>(&data)
                && filter.matches(&event)
            {
                count += 1;
            }
        }
        Ok(count)
    }
}
