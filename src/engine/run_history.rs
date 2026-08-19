use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use crate::engine::conversation_json::preflight_conversation_record_json;
use crate::engine::conversation_record::{
    validate_conversation_record_after_decode, validate_conversation_record_for_write,
};
use crate::engine::idempotency::{
    CONVERSATION_MESSAGE_OPERATION, ConversationIdempotencyCommit, IdempotencyClaim,
    IdempotencyClaimOutcome, IdempotencyCompletion, IdempotencyCompletionOutcome,
    IdempotencyLimits, IdempotencyLookup, IdempotencyQuotaResource, IdempotencyQuotaScope,
    IdempotencyRecord, IdempotencyState, IdempotencyUsage, PrincipalId, RUN_OPERATION,
    RunFenceHeartbeat, validate_digest,
};
use crate::engine::sessions::{
    ConversationRecord, ConversationSummary, DialogStateRecord, validate_session_id,
};
use crate::engine::task::TaskResult;
use crate::utils::error::{IronCrewError, Result};

pub use super::json_file_store::JsonFileStore;
use super::json_file_store_runtime::JsonFileStoreCore;
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
    if label == "conversation" {
        let json = std::str::from_utf8(&bytes).map_err(|error| {
            IronCrewError::Validation(format!("Serialized conversation was not UTF-8: {error}"))
        })?;
        preflight_conversation_record_json(json)?;
    }
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
pub(super) fn shared_json_run_lock(runs_dir: &Path) -> Arc<Mutex<()>> {
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

pub(super) fn validate_run_id(run_id: &str) -> Result<()> {
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
    preflight_conversation_record_json(&data)?;
    let record: ConversationRecord = serde_json::from_str(&data).map_err(|e| {
        IronCrewError::Validation(format!("Failed to parse conversation '{}': {}", id, e))
    })?;
    validate_conversation_record_after_decode(&record)?;
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
    preflight_conversation_record_json(&data).ok()?;
    let record = serde_json::from_str::<ConversationRecord>(&data).ok()?;
    validate_conversation_record_after_decode(&record).ok()?;
    Some(record)
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

fn idempotency_path(dir: &Path, key_hash: &str) -> Result<PathBuf> {
    validate_digest("idempotency key hash", key_hash)?;
    Ok(dir.join(format!("{key_hash}.json")))
}

fn read_idempotency_record(path: &Path) -> Result<IdempotencyRecord> {
    let data = read_json_record(path)?;
    let record: IdempotencyRecord = serde_json::from_str(&data).map_err(|error| {
        IronCrewError::Validation(format!(
            "Failed to parse idempotency record '{}': {error}",
            path.display()
        ))
    })?;
    record.validate()?;
    Ok(record)
}

fn parse_idempotency_timestamp(label: &str, value: &str) -> Result<chrono::DateTime<chrono::Utc>> {
    chrono::DateTime::parse_from_rfc3339(value)
        .map(|timestamp| timestamp.with_timezone(&chrono::Utc))
        .map_err(|error| IronCrewError::Validation(format!("{label} is not RFC3339: {error}")))
}

fn timestamp_has_passed(deadline: &str, now: &str) -> Result<bool> {
    Ok(
        parse_idempotency_timestamp("idempotency deadline", deadline)?
            <= parse_idempotency_timestamp("idempotency current time", now)?,
    )
}

fn earliest_deadline(current: Option<String>, candidate: &str) -> Result<Option<String>> {
    let candidate_time = parse_idempotency_timestamp("idempotency capacity deadline", candidate)?;
    match current {
        Some(current) => {
            let current_time =
                parse_idempotency_timestamp("idempotency capacity deadline", &current)?;
            Ok(Some(if current_time <= candidate_time {
                current
            } else {
                candidate.to_string()
            }))
        }
        None => Ok(Some(candidate.to_string())),
    }
}

fn retry_after_seconds(deadline: Option<&str>, now: &str) -> Result<u64> {
    let Some(deadline) = deadline else {
        return Ok(60);
    };
    let deadline = parse_idempotency_timestamp("idempotency capacity deadline", deadline)?;
    let now = parse_idempotency_timestamp("idempotency capacity clock", now)?;
    let milliseconds = deadline
        .signed_duration_since(now)
        .num_milliseconds()
        .max(1);
    Ok(u64::try_from(milliseconds.saturating_add(999) / 1_000)
        .unwrap_or(u64::MAX)
        .max(1))
}

fn quota_at_or_above(value: usize, limit: usize, percentage: usize) -> bool {
    let threshold = limit.saturating_mul(percentage).saturating_add(99) / 100;
    value >= threshold
}

fn json_recovery_grace_elapsed(
    hazard: &IdempotencyRecord,
    claim_time: &str,
    ttl: std::time::Duration,
) -> Result<bool> {
    let marked_at = hazard
        .completed_at
        .as_deref()
        .unwrap_or(hazard.updated_at.as_str());
    let marked_at = parse_idempotency_timestamp("idempotency hazard time", marked_at)?;
    let claim_time = parse_idempotency_timestamp("idempotency recovery claim time", claim_time)?;
    let grace = chrono::Duration::from_std(ttl).map_err(|error| {
        IronCrewError::Validation(format!(
            "Idempotency recovery grace is out of range: {error}"
        ))
    })?;
    let recovery_at = marked_at.checked_add_signed(grace).ok_or_else(|| {
        IronCrewError::Validation("Idempotency recovery grace deadline overflow".into())
    })?;
    Ok(claim_time >= recovery_at)
}

fn retention_expiry(now: &str, ttl_seconds: u64) -> Result<String> {
    let now = parse_idempotency_timestamp("idempotency current time", now)?;
    let ttl = i64::try_from(ttl_seconds)
        .map_err(|_| IronCrewError::Validation("Idempotency TTL is out of range".into()))?;
    now.checked_add_signed(chrono::Duration::seconds(ttl))
        .ok_or_else(|| IronCrewError::Validation("Idempotency retention expiry overflow".into()))
        .map(|timestamp| timestamp.to_rfc3339())
}

fn classify_idempotency_record(
    record: &IdempotencyRecord,
    request_fingerprint: &str,
    now: &str,
) -> Result<IdempotencyLookup> {
    validate_digest("request fingerprint", request_fingerprint)?;
    parse_idempotency_timestamp("idempotency current time", now)?;
    if record.request_fingerprint != request_fingerprint {
        return Ok(IdempotencyLookup::Conflict);
    }
    if record.state.is_terminal()
        && record
            .expires_at
            .as_deref()
            .is_some_and(|expires| timestamp_has_passed(expires, now).unwrap_or(false))
    {
        return Ok(IdempotencyLookup::Miss);
    }
    if record.state == IdempotencyState::Indeterminate {
        return Ok(IdempotencyLookup::Indeterminate(record.clone()));
    }
    if record.replayable() {
        return Ok(IdempotencyLookup::Replay(record.clone()));
    }
    if record.state.is_in_flight() && timestamp_has_passed(&record.lease_expires_at, now)? {
        return Ok(IdempotencyLookup::Indeterminate(record.clone()));
    }
    if record.state.is_in_flight() {
        return Ok(IdempotencyLookup::InProgress(record.clone()));
    }
    Ok(IdempotencyLookup::Indeterminate(record.clone()))
}

fn visit_idempotency_records(
    dir: &Path,
    mut visitor: impl FnMut(&Path, IdempotencyRecord) -> Result<()>,
) -> Result<()> {
    let mut scanned = 0usize;
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
            continue;
        }
        consume_scan_entry(&mut scanned)?;
        visitor(&path, read_idempotency_record(&path)?)?;
    }
    Ok(())
}

fn prune_json_idempotency_locked(dir: &Path, now: &str, limit: usize) -> Result<usize> {
    parse_idempotency_timestamp("idempotency prune time", now)?;
    if limit == 0 {
        return Ok(0);
    }
    let mut expired = Vec::new();
    visit_idempotency_records(dir, |path, record| {
        if record.state.is_terminal()
            && let Some(expires_at) = record.expires_at
            && timestamp_has_passed(&expires_at, now)?
        {
            expired.push((expires_at, path.to_path_buf()));
        }
        Ok(())
    })?;
    expired.sort_by(|left, right| left.0.cmp(&right.0));
    let mut removed = 0usize;
    for (_, path) in expired.into_iter().take(limit) {
        std::fs::remove_file(path)?;
        removed += 1;
    }
    Ok(removed)
}

fn json_idempotency_response_bytes(
    dir: &Path,
    principal_id: &PrincipalId,
    except_key: Option<&str>,
) -> Result<(usize, usize)> {
    let mut total = 0usize;
    let mut principal_total = 0usize;
    visit_idempotency_records(dir, |_, record| {
        if except_key != Some(record.key_hash.as_str()) {
            let bytes = record.response_body.as_deref().map(str::len).unwrap_or(0);
            total = total.checked_add(bytes).ok_or_else(|| {
                IronCrewError::Validation("Idempotency response byte total overflow".into())
            })?;
            if &record.principal_id == principal_id {
                principal_total = principal_total.checked_add(bytes).ok_or_else(|| {
                    IronCrewError::Validation(
                        "Principal idempotency response byte total overflow".into(),
                    )
                })?;
            }
        }
        Ok(())
    })?;
    Ok((total, principal_total))
}

fn terminalize_json_idempotency_indeterminate(
    dir: &Path,
    path: &Path,
    record: &mut IdempotencyRecord,
    completed_at: &str,
) -> Result<()> {
    record.state = IdempotencyState::Indeterminate;
    record.response_status = None;
    record.response_body = None;
    record.updated_at = completed_at.to_string();
    record.completed_at = Some(completed_at.to_string());
    record.expires_at = Some(retention_expiry(completed_at, record.ttl_seconds)?);
    record.validate()?;
    write_serialized_record_atomic(path, record, "idempotency record")?;
    // Keep the parameter explicit so callers cannot accidentally update a
    // record outside the store's idempotency directory.
    debug_assert!(path.starts_with(dir));
    Ok(())
}

fn active_conversation_idempotency_exists(
    dir: &Path,
    flow_path: Option<&str>,
    conversation_id: &str,
) -> Result<bool> {
    let flow_scope = flow_path.unwrap_or("");
    let mut found = false;
    visit_idempotency_records(dir, |_, record| {
        found |= record.operation == CONVERSATION_MESSAGE_OPERATION
            && record.scope == flow_scope
            && record.resource_id == conversation_id
            && record.state.is_in_flight();
        Ok(())
    })?;
    Ok(found)
}

fn ensure_idempotency_completion_fence(
    record: &IdempotencyRecord,
    completion: &IdempotencyCompletion,
) -> Result<()> {
    if record.principal_id != completion.principal_id
        || record.request_fingerprint != completion.request_fingerprint
        || record.attempt_id != completion.attempt_id
        || record.owner_instance_id != completion.owner_instance_id
    {
        return Err(IronCrewError::Conflict(
            "Idempotency operation changed before completion".into(),
        ));
    }
    Ok(())
}

fn transition_json_run_idempotency_to_running(dir: &Path, run: &RunRecord) -> Result<()> {
    visit_idempotency_records(dir, |path, mut record| {
        if record.operation == RUN_OPERATION
            && record.scope == run.flow
            && record.resource_id == run.run_id
            && record.owner_instance_id == run.owner_instance_id
            && record.state == IdempotencyState::Claimed
        {
            record.state = IdempotencyState::Running;
            record.lease_expires_at.clone_from(&run.lease_expires_at);
            record.updated_at.clone_from(&run.started_at);
            record.validate()?;
            write_serialized_record_atomic(path, &record, "running idempotency record")?;
        }
        Ok(())
    })
}

fn json_run_hydration_ledger(
    dir: &Path,
    run_id: &str,
    flow: &str,
    owner: &str,
) -> Result<Option<(PathBuf, IdempotencyRecord)>> {
    let mut matching = None;
    visit_idempotency_records(dir, |path, record| {
        if record.operation == RUN_OPERATION
            && record.scope == flow
            && record.resource_id == run_id
            && record.owner_instance_id == owner
            && matches!(
                record.state,
                IdempotencyState::Running | IdempotencyState::Completed
            )
        {
            if matching.is_some() {
                return Err(IronCrewError::Validation(format!(
                    "Run '{run_id}' has multiple matching idempotency ledgers"
                )));
            }
            matching = Some((path.to_path_buf(), record));
        }
        Ok(())
    })?;
    Ok(matching)
}

fn later_json_lease(existing: &str, proposed: &str) -> Result<String> {
    if existing.is_empty() {
        return Ok(proposed.to_string());
    }
    let existing_time = parse_idempotency_timestamp("existing run lease expiry", existing)?;
    let proposed_time = parse_idempotency_timestamp("proposed run lease expiry", proposed)?;
    Ok(if existing_time >= proposed_time {
        existing.to_string()
    } else {
        proposed.to_string()
    })
}

fn complete_json_run_idempotency(dir: &Path, run_id: &str, completed_at: &str) -> Result<()> {
    parse_idempotency_timestamp("run idempotency completion time", completed_at)?;
    visit_idempotency_records(dir, |path, mut record| {
        if record.operation == RUN_OPERATION
            && record.resource_id == run_id
            && matches!(
                record.state,
                IdempotencyState::Claimed
                    | IdempotencyState::Running
                    | IdempotencyState::Indeterminate
            )
        {
            record.state = IdempotencyState::Completed;
            record.lease_expires_at.clear();
            record.updated_at = completed_at.to_string();
            record.completed_at = Some(completed_at.to_string());
            record.expires_at = Some(retention_expiry(completed_at, record.ttl_seconds)?);
            record.validate()?;
            write_serialized_record_atomic(path, &record, "completed run idempotency record")?;
        }
        Ok(())
    })
}

#[async_trait]
impl StateStore for JsonFileStoreCore {
    async fn save_run_intent(&self, intent: RunIntent) -> Result<String> {
        if let Some(run_id) = intent.suggested_id.as_deref() {
            validate_run_id(run_id)?;
        }
        let may_hydrate = intent.suggested_id.is_some();
        let _guard = self
            .run_lock
            .lock()
            .map_err(|e| IronCrewError::Validation(format!("JSON run lock poisoned: {}", e)))?;
        let run_id = intent
            .suggested_id
            .clone()
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let filename = format!("{run_id}.json");
        let path = self.runs_dir.join(&filename);
        let proposed_lease = self.lease.deadline_now();

        if path.exists() {
            let duplicate_error =
                || IronCrewError::Validation(format!("Run '{run_id}' already exists"));
            if !may_hydrate {
                return Err(duplicate_error());
            }
            let data = read_json_record(&path)?;
            let mut existing: RunRecord = serde_json::from_str(&data).map_err(|error| {
                IronCrewError::Validation(format!("Failed to parse existing run: {error}"))
            })?;
            if !existing.status.is_in_flight()
                || existing.owner_instance_id != self.lease.instance_id()
                || existing.flow != intent.flow
            {
                return Err(duplicate_error());
            }
            let Some((ledger_path, mut ledger)) = json_run_hydration_ledger(
                &self.idempotency_dir,
                &run_id,
                &intent.flow,
                self.lease.instance_id(),
            )?
            else {
                return Err(duplicate_error());
            };
            let mut lease_expires_at =
                later_json_lease(&existing.lease_expires_at, &proposed_lease)?;
            if ledger.state == IdempotencyState::Running {
                lease_expires_at = later_json_lease(&lease_expires_at, &ledger.lease_expires_at)?;
                ledger.lease_expires_at.clone_from(&lease_expires_at);
                ledger.validate()?;
                write_serialized_record_atomic(
                    &ledger_path,
                    &ledger,
                    "hydrated run idempotency lease",
                )?;
            }
            existing.flow_name = intent.flow_name;
            existing.agent_count = intent.agent_count;
            existing.task_count = intent.task_count;
            existing.tags = intent.tags;
            existing.lease_expires_at = lease_expires_at;
            write_run_record_atomic(&path, &existing)?;
            tracing::debug!("Provisional run intent hydrated: {run_id}");
            return Ok(run_id);
        }

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
            lease_expires_at: proposed_lease,
        };
        write_serialized_record_create_new(&path, &record, "run intent").map_err(|error| {
            if matches!(&error, IronCrewError::Io(io) if io.kind() == std::io::ErrorKind::AlreadyExists)
            {
                IronCrewError::Validation(format!("Run '{}' already exists", run_id))
            } else {
                error
            }
        })?;
        if let Err(transition_error) =
            transition_json_run_idempotency_to_running(&self.idempotency_dir, &record)
        {
            let rollback = std::fs::remove_file(&path)
                .and_then(|()| std::fs::File::open(&self.runs_dir)?.sync_all());
            if let Err(rollback_error) = rollback {
                return Err(IronCrewError::Validation(format!(
                    "JSON run idempotency transition failed: {transition_error}; run rollback failed: {rollback_error}"
                )));
            }
            return Err(transition_error);
        }
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
        let transition = if record.status.is_terminal() {
            RunTransition::AlreadyTerminal(record.status.clone())
        } else {
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
            RunTransition::Applied
        };
        complete_json_run_idempotency(&self.idempotency_dir, run_id, &record.finished_at)?;
        tracing::info!("Run completion saved: {} ({})", run_id, record.status);
        Ok(transition)
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
        // Keep only small run identifiers and paths in memory. Storing full
        // records here could retain large task outputs for the entire scan.
        let mut candidates = Vec::new();
        let mut scanned = 0usize;
        for entry in std::fs::read_dir(&self.runs_dir)? {
            let path = entry?.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            consume_scan_entry(&mut scanned)?;
            let data = read_json_record(&path)?;
            let Ok(record) = serde_json::from_str::<RunRecord>(&data) else {
                continue;
            };
            if !record.status.is_in_flight() || record.owner_instance_id != self.lease.instance_id()
            {
                continue;
            }
            candidates.push((record.run_id, path, false));
        }
        candidates.sort_unstable_by(|left, right| left.0.cmp(&right.0));
        visit_idempotency_records(&self.idempotency_dir, |_, record| {
            if record.operation == RUN_OPERATION
                && let Ok(index) = candidates.binary_search_by(|candidate| {
                    candidate.0.as_str().cmp(record.resource_id.as_str())
                })
            {
                candidates[index].2 = true;
            }
            Ok(())
        })?;

        let mut count = 0;
        for (_, path, linked_to_ledger) in candidates {
            if linked_to_ledger {
                continue;
            }
            let data = read_json_record(&path)?;
            let Ok(mut record) = serde_json::from_str::<RunRecord>(&data) else {
                continue;
            };
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

        // The API persists the idempotency acceptance (including its run id)
        // before publishing the run intent. If the process dies in that
        // narrow window, materialize an observable Abandoned run instead of
        // replaying an id that can only return 404. Conversation mutations
        // cannot be reconstructed, so expired claims become indeterminate.
        visit_idempotency_records(&self.idempotency_dir, |path, mut idempotency| {
            if !idempotency.state.is_in_flight()
                || !timestamp_has_passed(&idempotency.lease_expires_at, now)?
            {
                return Ok(());
            }
            if idempotency.operation == RUN_OPERATION
                && idempotency.state == IdempotencyState::Claimed
            {
                validate_run_id(&idempotency.resource_id)?;
                let run_path = self
                    .runs_dir
                    .join(format!("{}.json", idempotency.resource_id));
                if !run_path.exists() {
                    let fallback = RunRecord {
                        run_id: idempotency.resource_id.clone(),
                        flow_name: idempotency.scope.clone(),
                        flow: idempotency.scope.clone(),
                        status: RunStatus::Abandoned,
                        started_at: idempotency.created_at.clone(),
                        finished_at: now.to_string(),
                        duration_ms: 0,
                        task_results: Vec::new(),
                        agent_count: 0,
                        task_count: 0,
                        total_tokens: 0,
                        cached_tokens: 0,
                        tags: Vec::new(),
                        owner_instance_id: idempotency.owner_instance_id.clone(),
                        lease_expires_at: String::new(),
                    };
                    write_serialized_record_create_new(
                        &run_path,
                        &fallback,
                        "abandoned idempotent run fallback",
                    )?;
                    count = count.saturating_add(1);
                }
            } else if idempotency.operation == CONVERSATION_MESSAGE_OPERATION {
                terminalize_json_idempotency_indeterminate(
                    &self.idempotency_dir,
                    path,
                    &mut idempotency,
                    now,
                )?;
            }
            Ok(())
        })?;

        let mut terminal_runs = std::collections::HashMap::<String, String>::new();
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
            if record.status.is_in_flight()
                && lease_is_expired(&record.lease_expires_at, now_parsed)
            {
                record.status = RunStatus::Abandoned;
                record.finished_at = now.to_string();
                record.lease_expires_at.clear();
                write_run_record_atomic(&path, &record)?;
                count = count.saturating_add(1);
            }
            if record.status.is_terminal() {
                terminal_runs.insert(record.run_id, record.finished_at);
            }
        }

        visit_idempotency_records(&self.idempotency_dir, |path, mut idempotency| {
            if idempotency.operation != RUN_OPERATION
                || !matches!(
                    idempotency.state,
                    IdempotencyState::Claimed
                        | IdempotencyState::Running
                        | IdempotencyState::Indeterminate
                )
            {
                return Ok(());
            }
            let Some(finished_at) = terminal_runs.get(&idempotency.resource_id) else {
                return Ok(());
            };
            parse_idempotency_timestamp("run idempotency completion time", finished_at)?;
            idempotency.state = IdempotencyState::Completed;
            idempotency.lease_expires_at.clear();
            idempotency.updated_at.clone_from(finished_at);
            idempotency.completed_at = Some(finished_at.clone());
            idempotency.expires_at = Some(retention_expiry(finished_at, idempotency.ttl_seconds)?);
            idempotency.validate()?;
            write_serialized_record_atomic(path, &idempotency, "reconciled run idempotency")
        })?;
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
    async fn lookup_idempotency_for_principal(
        &self,
        principal_id: &PrincipalId,
        key_hash: &str,
        request_fingerprint: &str,
        now: &str,
    ) -> Result<IdempotencyLookup> {
        let path = idempotency_path(&self.idempotency_dir, key_hash)?;
        let _guard = self.run_lock.lock().map_err(|error| {
            IronCrewError::Validation(format!("JSON store lock error: {error}"))
        })?;
        if !path.exists() {
            return Ok(IdempotencyLookup::Miss);
        }
        let record = read_idempotency_record(&path)?;
        if &record.principal_id != principal_id {
            return Ok(IdempotencyLookup::Conflict);
        }
        classify_idempotency_record(&record, request_fingerprint, now)
    }

    async fn claim_idempotency_with_limits(
        &self,
        claim: IdempotencyClaim,
        limits: IdempotencyLimits,
    ) -> Result<IdempotencyClaimOutcome> {
        claim.validate()?;
        limits.validate()?;
        let _guard = self.run_lock.lock().map_err(|error| {
            IronCrewError::Validation(format!("JSON store lock error: {error}"))
        })?;
        prune_json_idempotency_locked(
            &self.idempotency_dir,
            &claim.created_at,
            limits.prune_batch,
        )?;

        let path = idempotency_path(&self.idempotency_dir, &claim.key_hash)?;
        if path.exists() {
            let mut existing = read_idempotency_record(&path)?;
            let expired_terminal = existing.state.is_terminal()
                && match existing.expires_at.as_deref() {
                    Some(expires) => timestamp_has_passed(expires, &claim.created_at)?,
                    None => false,
                };
            if expired_terminal {
                std::fs::remove_file(&path)?;
            } else if existing.principal_id != claim.principal_id
                || existing.request_fingerprint != claim.request_fingerprint
            {
                return Ok(IdempotencyClaimOutcome::Conflict);
            } else if existing.state == IdempotencyState::Indeterminate {
                return Ok(IdempotencyClaimOutcome::Indeterminate(existing));
            } else if existing.replayable() {
                return Ok(IdempotencyClaimOutcome::Replay(existing));
            } else if existing.state.is_in_flight()
                && timestamp_has_passed(&existing.lease_expires_at, &claim.created_at)?
            {
                terminalize_json_idempotency_indeterminate(
                    &self.idempotency_dir,
                    &path,
                    &mut existing,
                    &claim.created_at,
                )?;
                return Ok(IdempotencyClaimOutcome::Indeterminate(existing));
            } else if existing.state.is_in_flight() {
                return Ok(IdempotencyClaimOutcome::InProgress(existing));
            } else {
                return Ok(IdempotencyClaimOutcome::Indeterminate(existing));
            }
        }

        let mut recovery_hazard = None;
        if let Some(exclusive_scope) = claim.exclusive_scope.as_deref() {
            let mut busy = false;
            visit_idempotency_records(&self.idempotency_dir, |existing_path, mut existing| {
                if existing.exclusive_scope.as_deref() != Some(exclusive_scope)
                    || !existing.state.is_in_flight()
                {
                    return Ok(());
                }
                if timestamp_has_passed(&existing.lease_expires_at, &claim.created_at)? {
                    terminalize_json_idempotency_indeterminate(
                        &self.idempotency_dir,
                        existing_path,
                        &mut existing,
                        &claim.created_at,
                    )?;
                    // Do not launch a fresh turn in the same critical section
                    // that discovers an expired predecessor. Its worker may
                    // still be unwinding after losing the lease.
                    busy = true;
                } else {
                    busy = true;
                }
                Ok(())
            })?;
            if busy {
                return Ok(IdempotencyClaimOutcome::Busy);
            }

            let mut hazards = Vec::new();
            visit_idempotency_records(&self.idempotency_dir, |existing_path, existing| {
                if existing.exclusive_scope.as_deref() == Some(exclusive_scope)
                    && existing.state == IdempotencyState::Indeterminate
                {
                    hazards.push((existing_path.to_path_buf(), existing));
                }
                Ok(())
            })?;
            if !hazards.is_empty() {
                let acknowledged = hazards.len() == 1
                    && hazards[0].1.principal_id == claim.principal_id
                    && claim.recovery_key_hash.as_deref() == Some(hazards[0].1.key_hash.as_str());
                if !acknowledged
                    || !json_recovery_grace_elapsed(
                        &hazards[0].1,
                        &claim.created_at,
                        self.lease.ttl(),
                    )?
                {
                    return Ok(IdempotencyClaimOutcome::Busy);
                }
                recovery_hazard = hazards.pop();
            }
        }

        if claim.operation == CONVERSATION_MESSAGE_OPERATION {
            validate_session_id(&claim.resource_id)?;
            let expected_revision = claim.base_revision.ok_or_else(|| {
                IronCrewError::Validation(
                    "Conversation idempotency claim has no base revision".into(),
                )
            })?;
            let conversation_path = conversation_file_path(
                &self.conversations_dir,
                Some(&claim.scope),
                &claim.resource_id,
            );
            let current = load_conversation_file(&conversation_path, &claim.resource_id)?;
            let valid = current.as_ref().is_some_and(|conversation| {
                let expected_scope = super::sessions::conversation_mutation_scope(
                    &claim.scope,
                    &claim.resource_id,
                    &conversation.execution.incarnation_id,
                );
                conversation.execution.validate().is_ok()
                    && conversation.revision == expected_revision
                    && claim.exclusive_scope.as_deref() == Some(expected_scope.as_str())
            });
            if !valid {
                return Ok(IdempotencyClaimOutcome::Conflict);
            }
        }

        let mut record_count = 0usize;
        let mut principal_record_count = 0usize;
        let mut principal_in_flight = 0usize;
        let mut stored_response_bytes = 0usize;
        let mut principal_response_bytes = 0usize;
        let mut next_global_capacity = None;
        let mut next_principal_capacity = None;
        visit_idempotency_records(&self.idempotency_dir, |_, record| {
            record_count = record_count.saturating_add(1);
            let response_bytes = record.response_body.as_deref().map(str::len).unwrap_or(0);
            stored_response_bytes = stored_response_bytes
                .checked_add(response_bytes)
                .ok_or_else(|| {
                    IronCrewError::Validation("Idempotency response byte total overflow".into())
                })?;
            let deadline = if record.state.is_in_flight() {
                Some(record.lease_expires_at.as_str())
            } else {
                record.expires_at.as_deref()
            };
            if let Some(deadline) = deadline {
                next_global_capacity = earliest_deadline(next_global_capacity.take(), deadline)?;
            }
            if record.principal_id == claim.principal_id {
                principal_record_count = principal_record_count.saturating_add(1);
                principal_in_flight =
                    principal_in_flight.saturating_add(usize::from(record.state.is_in_flight()));
                principal_response_bytes = principal_response_bytes
                    .checked_add(response_bytes)
                    .ok_or_else(|| {
                        IronCrewError::Validation(
                            "Principal idempotency response byte total overflow".into(),
                        )
                    })?;
                if let Some(deadline) = deadline {
                    next_principal_capacity =
                        earliest_deadline(next_principal_capacity.take(), deadline)?;
                }
            }
            Ok(())
        })?;
        if record_count >= limits.global_max_records {
            return Ok(IdempotencyClaimOutcome::QuotaExceeded {
                scope: IdempotencyQuotaScope::Global,
                resource: IdempotencyQuotaResource::Records,
                retry_after_seconds: retry_after_seconds(
                    next_global_capacity.as_deref(),
                    &claim.created_at,
                )?,
            });
        }
        if principal_record_count >= limits.principal_max_records {
            return Ok(IdempotencyClaimOutcome::QuotaExceeded {
                scope: IdempotencyQuotaScope::Principal,
                resource: IdempotencyQuotaResource::Records,
                retry_after_seconds: retry_after_seconds(
                    next_principal_capacity.as_deref(),
                    &claim.created_at,
                )?,
            });
        }
        if principal_in_flight >= limits.principal_max_in_flight {
            return Ok(IdempotencyClaimOutcome::QuotaExceeded {
                scope: IdempotencyQuotaScope::Principal,
                resource: IdempotencyQuotaResource::InFlight,
                retry_after_seconds: retry_after_seconds(
                    next_principal_capacity.as_deref(),
                    &claim.created_at,
                )?,
            });
        }
        let mut record = claim.to_record();
        record.response_body = record.response_body.filter(|body| {
            let global_fits = stored_response_bytes
                .checked_add(body.len())
                .is_some_and(|total| total <= limits.global_max_response_bytes);
            let principal_fits = principal_response_bytes
                .checked_add(body.len())
                .is_some_and(|total| total <= limits.principal_max_response_bytes);
            global_fits && principal_fits
        });
        record.validate()?;
        write_serialized_record_create_new(&path, &record, "idempotency claim")?;
        if let Some((hazard_path, mut hazard)) = recovery_hazard {
            let original_hazard = hazard.clone();
            hazard.exclusive_scope = None;
            hazard.updated_at.clone_from(&claim.created_at);
            hazard.validate()?;
            if let Err(recovery_error) = write_serialized_record_atomic(
                &hazard_path,
                &hazard,
                "recovered idempotency hazard",
            ) {
                let claim_rollback = std::fs::remove_file(&path)
                    .and_then(|()| std::fs::File::open(&self.idempotency_dir)?.sync_all());
                let hazard_rollback = write_serialized_record_atomic(
                    &hazard_path,
                    &original_hazard,
                    "idempotency hazard recovery rollback",
                );
                if let Err(rollback_error) = claim_rollback {
                    return Err(IronCrewError::Validation(format!(
                        "JSON idempotency hazard recovery failed: {recovery_error}; claim rollback failed: {rollback_error}"
                    )));
                }
                if let Err(rollback_error) = hazard_rollback {
                    return Err(IronCrewError::Validation(format!(
                        "JSON idempotency hazard recovery failed: {recovery_error}; hazard rollback failed: {rollback_error}"
                    )));
                }
                return Err(recovery_error);
            }
        }
        Ok(IdempotencyClaimOutcome::Claimed(record))
    }

    async fn heartbeat_idempotency(
        &self,
        key_hash: &str,
        attempt_id: &str,
        new_lease_expires_at: &str,
    ) -> Result<bool> {
        parse_idempotency_timestamp("idempotency lease expiry", new_lease_expires_at)?;
        let path = idempotency_path(&self.idempotency_dir, key_hash)?;
        let _guard = self.run_lock.lock().map_err(|error| {
            IronCrewError::Validation(format!("JSON store lock error: {error}"))
        })?;
        if !path.exists() {
            return Ok(false);
        }
        let mut record = read_idempotency_record(&path)?;
        if record.attempt_id != attempt_id {
            return Err(IronCrewError::Conflict(
                "Idempotency attempt changed before heartbeat".into(),
            ));
        }
        if !record.state.is_in_flight() {
            return Ok(record.state == IdempotencyState::Completed);
        }
        record.lease_expires_at = new_lease_expires_at.to_string();
        record.validate()?;
        write_serialized_record_atomic(&path, &record, "idempotency heartbeat")?;
        Ok(true)
    }

    async fn heartbeat_idempotent_run(
        &self,
        run_id: &str,
        key_hash: &str,
        attempt_id: &str,
        new_lease_expires_at: &str,
    ) -> Result<RunFenceHeartbeat> {
        validate_run_id(run_id)?;
        parse_idempotency_timestamp("idempotency run lease expiry", new_lease_expires_at)?;
        let ledger_path = idempotency_path(&self.idempotency_dir, key_hash)?;
        let _guard = self.run_lock.lock().map_err(|error| {
            IronCrewError::Validation(format!("JSON store lock error: {error}"))
        })?;
        if !ledger_path.exists() {
            return Ok(RunFenceHeartbeat::Lost);
        }
        let mut ledger = read_idempotency_record(&ledger_path)?;
        if ledger.attempt_id != attempt_id {
            return Err(IronCrewError::Conflict(
                "Idempotency attempt changed before run heartbeat".into(),
            ));
        }
        if ledger.operation != RUN_OPERATION
            || ledger.resource_id != run_id
            || ledger.owner_instance_id != self.lease.instance_id()
        {
            return Ok(RunFenceHeartbeat::Lost);
        }

        let run_path = self.runs_dir.join(format!("{run_id}.json"));
        if !run_path.exists() {
            if ledger.state != IdempotencyState::Claimed {
                return Ok(RunFenceHeartbeat::Lost);
            }
            ledger.lease_expires_at = new_lease_expires_at.to_string();
            ledger.validate()?;
            write_serialized_record_atomic(
                &ledger_path,
                &ledger,
                "claimed run idempotency heartbeat",
            )?;
            return Ok(RunFenceHeartbeat::Owned);
        }

        let data = read_json_record(&run_path)?;
        let mut run: RunRecord = serde_json::from_str(&data).map_err(|error| {
            IronCrewError::Validation(format!("Failed to parse heartbeat run: {error}"))
        })?;
        if run.owner_instance_id != ledger.owner_instance_id {
            return Ok(RunFenceHeartbeat::Lost);
        }
        if run.status.is_terminal() {
            return if ledger.state == IdempotencyState::Indeterminate {
                Ok(RunFenceHeartbeat::Lost)
            } else {
                Ok(RunFenceHeartbeat::Terminal(run.status))
            };
        }
        if ledger.state != IdempotencyState::Running {
            return Ok(RunFenceHeartbeat::Lost);
        }

        let original_run = run.clone();
        run.lease_expires_at = new_lease_expires_at.to_string();
        ledger.lease_expires_at = new_lease_expires_at.to_string();
        ledger.validate()?;
        write_run_record_atomic(&run_path, &run)?;
        if let Err(heartbeat_error) = write_serialized_record_atomic(
            &ledger_path,
            &ledger,
            "running run idempotency heartbeat",
        ) {
            if let Err(rollback_error) = write_run_record_atomic(&run_path, &original_run) {
                return Err(IronCrewError::Validation(format!(
                    "JSON idempotent run heartbeat failed: {heartbeat_error}; run rollback failed: {rollback_error}"
                )));
            }
            return Err(heartbeat_error);
        }
        Ok(RunFenceHeartbeat::Owned)
    }

    async fn complete_idempotency_with_limits(
        &self,
        completion: IdempotencyCompletion,
        limits: IdempotencyLimits,
    ) -> Result<IdempotencyCompletionOutcome> {
        completion.validate()?;
        limits.validate()?;
        let path = idempotency_path(&self.idempotency_dir, &completion.key_hash)?;
        let _guard = self.run_lock.lock().map_err(|error| {
            IronCrewError::Validation(format!("JSON store lock error: {error}"))
        })?;
        if !path.exists() {
            return Err(IronCrewError::Validation(
                "Idempotency claim not found during completion".into(),
            ));
        }
        let mut record = read_idempotency_record(&path)?;
        ensure_idempotency_completion_fence(&record, &completion)?;
        if record.state == IdempotencyState::Completed {
            return Ok(IdempotencyCompletionOutcome {
                replayable: record.replayable(),
                already_completed: true,
            });
        }
        if record.state == IdempotencyState::Indeterminate {
            return Err(IronCrewError::Conflict(
                "Indeterminate idempotency outcomes cannot be completed".into(),
            ));
        }

        let (stored_bytes, principal_stored_bytes) = json_idempotency_response_bytes(
            &self.idempotency_dir,
            &completion.principal_id,
            Some(completion.key_hash.as_str()),
        )?;
        let response_body = completion.response_body.filter(|body| {
            let global_fits = stored_bytes
                .checked_add(body.len())
                .is_some_and(|total| total <= limits.global_max_response_bytes);
            let principal_fits = principal_stored_bytes
                .checked_add(body.len())
                .is_some_and(|total| total <= limits.principal_max_response_bytes);
            global_fits && principal_fits
        });
        record.state = IdempotencyState::Completed;
        record.response_status = Some(completion.response_status);
        record.response_body = response_body;
        record.updated_at = completion.completed_at.clone();
        record.completed_at = Some(completion.completed_at);
        record.expires_at = Some(completion.expires_at);
        record.validate()?;
        write_serialized_record_atomic(&path, &record, "idempotency completion")?;
        Ok(IdempotencyCompletionOutcome {
            replayable: record.replayable(),
            already_completed: false,
        })
    }

    async fn commit_conversation_idempotency_with_limits(
        &self,
        completion: IdempotencyCompletion,
        conversation: &ConversationRecord,
        limits: IdempotencyLimits,
    ) -> Result<ConversationIdempotencyCommit> {
        completion.validate()?;
        limits.validate()?;
        validate_conversation_record_for_write(conversation)?;
        let ledger_path = idempotency_path(&self.idempotency_dir, &completion.key_hash)?;
        let _guard = self.run_lock.lock().map_err(|error| {
            IronCrewError::Validation(format!("JSON store lock error: {error}"))
        })?;
        if !ledger_path.exists() {
            return Err(IronCrewError::Validation(
                "Idempotency claim not found during conversation commit".into(),
            ));
        }
        let idempotency = read_idempotency_record(&ledger_path)?;
        ensure_idempotency_completion_fence(&idempotency, &completion)?;
        let expected_scope = super::sessions::conversation_mutation_scope(
            conversation.flow_path.as_deref().unwrap_or(""),
            &conversation.id,
            &conversation.execution.incarnation_id,
        );
        if idempotency.operation != CONVERSATION_MESSAGE_OPERATION
            || idempotency.resource_id != conversation.id
            || idempotency.scope != conversation.flow_path.as_deref().unwrap_or("")
            || idempotency.exclusive_scope.as_deref() != Some(expected_scope.as_str())
        {
            return Err(IronCrewError::Conflict(
                "Idempotency claim does not match the conversation scope".into(),
            ));
        }
        let expected_revision = idempotency.base_revision.ok_or_else(|| {
            IronCrewError::Validation("Conversation idempotency claim has no base revision".into())
        })?;
        if expected_revision != conversation.revision {
            return Err(IronCrewError::Conflict(format!(
                "Conversation '{}' changed before idempotent commit",
                conversation.id
            )));
        }
        if idempotency.state == IdempotencyState::Completed {
            return Ok(ConversationIdempotencyCommit {
                revision: expected_revision.saturating_add(1),
                replayable: idempotency.replayable(),
                already_completed: true,
            });
        }
        if idempotency.state == IdempotencyState::Indeterminate {
            return Err(IronCrewError::Conflict(
                "Indeterminate conversation outcomes cannot be committed".into(),
            ));
        }

        let conversation_path = conversation_file_path(
            &self.conversations_dir,
            conversation.flow_path.as_deref(),
            &conversation.id,
        );
        let current = load_conversation_file(&conversation_path, &conversation.id)?;
        if !current.as_ref().is_some_and(|saved| {
            saved.revision == expected_revision && saved.execution == conversation.execution
        }) {
            return Err(IronCrewError::Conflict(format!(
                "Conversation '{}' changed since revision {expected_revision}",
                conversation.id
            )));
        }
        let next_revision = expected_revision
            .checked_add(1)
            .ok_or_else(|| IronCrewError::Validation("Conversation revision overflow".into()))?;
        let (stored_bytes, principal_stored_bytes) = json_idempotency_response_bytes(
            &self.idempotency_dir,
            &completion.principal_id,
            Some(completion.key_hash.as_str()),
        )?;
        let response_body = completion.response_body.filter(|body| {
            let global_fits = stored_bytes
                .checked_add(body.len())
                .is_some_and(|total| total <= limits.global_max_response_bytes);
            let principal_fits = principal_stored_bytes
                .checked_add(body.len())
                .is_some_and(|total| total <= limits.principal_max_response_bytes);
            global_fits && principal_fits
        });

        let mut conversation_to_write = conversation.clone();
        conversation_to_write.revision = next_revision;
        let mut idempotency_to_write = idempotency;
        idempotency_to_write.state = IdempotencyState::Completed;
        idempotency_to_write.response_status = Some(completion.response_status);
        idempotency_to_write.response_body = response_body;
        idempotency_to_write.updated_at = completion.completed_at.clone();
        idempotency_to_write.completed_at = Some(completion.completed_at);
        idempotency_to_write.expires_at = Some(completion.expires_at);
        idempotency_to_write.validate()?;

        // Both files share one process-wide critical section. JSON remains a
        // single-process backend; SQL backends provide the crash-atomic form.
        write_serialized_record_atomic(&conversation_path, &conversation_to_write, "conversation")?;
        write_serialized_record_atomic(
            &ledger_path,
            &idempotency_to_write,
            "idempotency completion",
        )?;
        Ok(ConversationIdempotencyCommit {
            revision: next_revision,
            replayable: idempotency_to_write.replayable(),
            already_completed: false,
        })
    }

    async fn mark_idempotency_indeterminate(
        &self,
        key_hash: &str,
        attempt_id: &str,
        completed_at: &str,
        expires_at: &str,
    ) -> Result<bool> {
        parse_idempotency_timestamp("idempotency completion time", completed_at)?;
        parse_idempotency_timestamp("idempotency retention expiry", expires_at)?;
        let path = idempotency_path(&self.idempotency_dir, key_hash)?;
        let _guard = self.run_lock.lock().map_err(|error| {
            IronCrewError::Validation(format!("JSON store lock error: {error}"))
        })?;
        if !path.exists() {
            return Ok(false);
        }
        let mut record = read_idempotency_record(&path)?;
        if record.attempt_id != attempt_id {
            return Err(IronCrewError::Conflict(
                "Idempotency attempt changed before indeterminate transition".into(),
            ));
        }
        if record.state.is_terminal() {
            return Ok(false);
        }
        record.state = IdempotencyState::Indeterminate;
        record.response_status = None;
        record.response_body = None;
        record.updated_at = completed_at.to_string();
        record.completed_at = Some(completed_at.to_string());
        record.expires_at = Some(expires_at.to_string());
        record.validate()?;
        write_serialized_record_atomic(&path, &record, "indeterminate idempotency record")?;
        Ok(true)
    }

    async fn release_idempotency(&self, key_hash: &str, attempt_id: &str) -> Result<bool> {
        let path = idempotency_path(&self.idempotency_dir, key_hash)?;
        let _guard = self.run_lock.lock().map_err(|error| {
            IronCrewError::Validation(format!("JSON store lock error: {error}"))
        })?;
        if !path.exists() {
            return Ok(false);
        }
        let record = read_idempotency_record(&path)?;
        if record.attempt_id != attempt_id {
            return Err(IronCrewError::Conflict(
                "Idempotency attempt changed before release".into(),
            ));
        }
        if !record.state.is_in_flight() {
            return Ok(false);
        }
        std::fs::remove_file(path)?;
        Ok(true)
    }

    async fn prune_idempotency(&self, now: &str, limit: usize) -> Result<usize> {
        let _guard = self.run_lock.lock().map_err(|error| {
            IronCrewError::Validation(format!("JSON store lock error: {error}"))
        })?;
        prune_json_idempotency_locked(&self.idempotency_dir, now, limit)
    }

    async fn idempotency_usage(
        &self,
        principal_id: &PrincipalId,
        limits: IdempotencyLimits,
    ) -> Result<IdempotencyUsage> {
        limits.validate()?;
        let _guard = self.run_lock.lock().map_err(|error| {
            IronCrewError::Validation(format!("JSON store lock error: {error}"))
        })?;
        let mut by_principal =
            std::collections::HashMap::<PrincipalId, (usize, usize, usize)>::new();
        visit_idempotency_records(&self.idempotency_dir, |_, record| {
            let usage = by_principal.entry(record.principal_id).or_default();
            usage.0 = usage.0.saturating_add(1);
            usage.1 = usage
                .1
                .saturating_add(usize::from(record.state.is_in_flight()));
            usage.2 = usage
                .2
                .checked_add(record.response_body.as_deref().map(str::len).unwrap_or(0))
                .ok_or_else(|| {
                    IronCrewError::Validation("Idempotency response byte total overflow".into())
                })?;
            Ok(())
        })?;

        let mut snapshot = IdempotencyUsage {
            principal_count: by_principal.len(),
            ..IdempotencyUsage::default()
        };
        for (id, (records, in_flight, response_bytes)) in by_principal {
            snapshot.global_records = snapshot.global_records.saturating_add(records);
            snapshot.global_in_flight = snapshot.global_in_flight.saturating_add(in_flight);
            snapshot.global_response_bytes = snapshot
                .global_response_bytes
                .checked_add(response_bytes)
                .ok_or_else(|| {
                    IronCrewError::Validation("Idempotency response byte total overflow".into())
                })?;
            snapshot.max_principal_records = snapshot.max_principal_records.max(records);
            snapshot.max_principal_in_flight = snapshot.max_principal_in_flight.max(in_flight);
            snapshot.max_principal_response_bytes =
                snapshot.max_principal_response_bytes.max(response_bytes);
            if &id == principal_id {
                snapshot.principal_records = records;
                snapshot.principal_in_flight = in_flight;
                snapshot.principal_response_bytes = response_bytes;
            }
            let at = |threshold: usize| {
                quota_at_or_above(records, limits.principal_max_records, threshold)
                    || quota_at_or_above(in_flight, limits.principal_max_in_flight, threshold)
                    || quota_at_or_above(
                        response_bytes,
                        limits.principal_max_response_bytes,
                        threshold,
                    )
            };
            snapshot.principals_at_or_above_80_percent += usize::from(at(80));
            snapshot.principals_at_or_above_90_percent += usize::from(at(90));
            snapshot.principals_at_or_above_100_percent += usize::from(at(100));
        }
        Ok(snapshot)
    }

    // `get_*` returns Ok(None) when the file is missing so the caller can
    // tell "first time this id is used" apart from real I/O errors.

    async fn save_conversation(&self, record: &ConversationRecord) -> Result<u64> {
        validate_conversation_record_for_write(record)?;
        let _guard = self.run_lock.lock().map_err(|error| {
            IronCrewError::Validation(format!("JSON store lock error: {error}"))
        })?;
        if active_conversation_idempotency_exists(
            &self.idempotency_dir,
            record.flow_path.as_deref(),
            &record.id,
        )? {
            return Err(IronCrewError::Conflict(format!(
                "Conversation '{}' has an active idempotent message operation",
                record.id
            )));
        }
        // Scope the on-disk filename by flow to prevent two flows sharing
        // the same `id` from clobbering each other. Legacy records
        // (flow_path = None) keep the old `<id>.json` layout.
        let path = conversation_file_path(
            &self.conversations_dir,
            record.flow_path.as_deref(),
            &record.id,
        );
        let current = load_conversation_file(&path, &record.id)?;
        if current.as_ref().is_some_and(|saved| {
            saved.revision != record.revision || saved.execution != record.execution
        }) || current.is_none() && record.revision != 0
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
        let _guard = self.run_lock.lock().map_err(|error| {
            IronCrewError::Validation(format!("JSON store lock error: {error}"))
        })?;
        if active_conversation_idempotency_exists(&self.idempotency_dir, flow_path, id)? {
            return Err(IronCrewError::Conflict(format!(
                "Conversation '{id}' has an active idempotent message operation"
            )));
        }
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
