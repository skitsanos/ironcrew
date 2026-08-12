//! Storage-neutral domain types for durable run-event journals.
//!
//! The live [`super::eventbus::EventBus`] remains responsible for producing
//! events. This module defines the bounded persistence contract used by shared
//! stores and the opaque cursor exposed through Server-Sent Events. It contains
//! no storage or HTTP implementation so local stores can retain their existing
//! process-scoped behavior.

use std::fmt;
use std::str::FromStr;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::run_event_timing::RunEventWriteTiming;
use super::run_history::{RunStatus, validate_run_id};
use crate::utils::error::{IronCrewError, Result};

// Keep the per-run journal defaults and hard ceilings aligned with EventBus.
pub const DEFAULT_MAX_EVENTS_PER_RUN: usize = 1_000;
pub const HARD_MAX_EVENTS_PER_RUN: usize = 10_000;
pub const DEFAULT_MAX_BYTES_PER_RUN: usize = 4 * 1024 * 1024;
pub const HARD_MAX_BYTES_PER_RUN: usize = 64 * 1024 * 1024;
pub const DEFAULT_MAX_EVENT_BYTES: usize = 256 * 1024;
pub const HARD_MAX_EVENT_BYTES: usize = 16 * 1024 * 1024;

pub const DEFAULT_JOURNAL_RETENTION_SECS: u64 = 60 * 60;
pub const MIN_JOURNAL_RETENTION_SECS: u64 = 60;
pub const MAX_JOURNAL_RETENTION_SECS: u64 = 30 * 24 * 60 * 60;
pub const DEFAULT_JOURNAL_MAX_TOTAL_EVENTS: u64 = 100_000;
pub const HARD_JOURNAL_MAX_TOTAL_EVENTS: u64 = 10_000_000;
pub const DEFAULT_JOURNAL_MAX_TOTAL_BYTES: u64 = 256 * 1024 * 1024;
pub const HARD_JOURNAL_MAX_TOTAL_BYTES: u64 = 8 * 1024 * 1024 * 1024;
pub const DEFAULT_JOURNAL_PAGE_MAX_EVENTS: usize = 64;
pub const DEFAULT_JOURNAL_PAGE_MAX_BYTES: usize = 512 * 1024;
pub const HARD_JOURNAL_PAGE_MAX_BYTES: usize = HARD_MAX_BYTES_PER_RUN;
pub const DEFAULT_JOURNAL_POLL_INTERVAL_MS: u64 = 500;
pub const MIN_JOURNAL_POLL_INTERVAL_MS: u64 = 100;
pub const MAX_JOURNAL_POLL_INTERVAL_MS: u64 = 5_000;
pub const DEFAULT_JOURNAL_READ_TIMEOUT_MS: u64 = 2_000;
pub const MIN_JOURNAL_READ_TIMEOUT_MS: u64 = 100;
pub const MAX_JOURNAL_READ_TIMEOUT_MS: u64 = 30_000;
pub const DEFAULT_JOURNAL_WRITE_TIMEOUT_MS: u64 = 1_500;
pub const MIN_JOURNAL_WRITE_TIMEOUT_MS: u64 = 100;
pub const MAX_JOURNAL_WRITE_TIMEOUT_MS: u64 = 5_000;
pub const DEFAULT_JOURNAL_PRUNE_BATCH: usize = 1_000;
pub const HARD_JOURNAL_PRUNE_BATCH: usize = 10_000;

const MIN_EVENT_BYTES: usize = 1024;
const MIN_PAGE_BYTES: usize = 1024;
const MAX_RUN_ID_BYTES: usize = 128;
const MAX_SEQUENCE_DIGITS: usize = 20;
pub const MAX_RUN_EVENT_CURSOR_BYTES: usize = MAX_RUN_ID_BYTES + 1 + MAX_SEQUENCE_DIGITS;
const MAX_EVENT_TYPE_BYTES: usize = 64;
const MAX_FLOW_BYTES: usize = 255;
const MAX_OWNER_INSTANCE_ID_BYTES: usize = 255;

/// Whether a configured store can replay a run's events across processes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventJournalScope {
    #[default]
    ProcessLocal,
    SharedStore,
}

/// Immutable resource policy for a run-event journal.
///
/// Values are read once at server boot. Per-run limits deliberately reuse the
/// EventBus environment variables so enabling durability does not create a
/// second, contradictory event-size policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunEventJournalConfig {
    pub max_events_per_run: usize,
    pub max_bytes_per_run: usize,
    pub max_event_bytes: usize,
    pub retention: Duration,
    pub max_total_events: u64,
    pub max_total_bytes: u64,
    /// Derived from `max_events_per_run`, with a default ceiling of 64 rows.
    pub page_max_events: usize,
    pub page_max_bytes: usize,
    pub poll_interval: Duration,
    /// Per-page database deadline applied by SSE handlers. It bounds pool and
    /// network stalls independently from the client connection lifetime.
    pub read_timeout: Duration,
    /// Outer deadline for one PostgreSQL append attempt, including pool
    /// acquisition and the complete transaction. Database statements receive
    /// a smaller derived timeout so lock waits end before this deadline.
    pub write_timeout: Duration,
    pub prune_batch: usize,
}

impl Default for RunEventJournalConfig {
    fn default() -> Self {
        Self {
            max_events_per_run: DEFAULT_MAX_EVENTS_PER_RUN,
            max_bytes_per_run: DEFAULT_MAX_BYTES_PER_RUN,
            max_event_bytes: DEFAULT_MAX_EVENT_BYTES,
            retention: Duration::from_secs(DEFAULT_JOURNAL_RETENTION_SECS),
            max_total_events: DEFAULT_JOURNAL_MAX_TOTAL_EVENTS,
            max_total_bytes: DEFAULT_JOURNAL_MAX_TOTAL_BYTES,
            page_max_events: DEFAULT_JOURNAL_PAGE_MAX_EVENTS,
            page_max_bytes: DEFAULT_JOURNAL_PAGE_MAX_BYTES,
            poll_interval: Duration::from_millis(DEFAULT_JOURNAL_POLL_INTERVAL_MS),
            read_timeout: Duration::from_millis(DEFAULT_JOURNAL_READ_TIMEOUT_MS),
            write_timeout: Duration::from_millis(DEFAULT_JOURNAL_WRITE_TIMEOUT_MS),
            prune_batch: DEFAULT_JOURNAL_PRUNE_BATCH,
        }
    }
}

impl RunEventJournalConfig {
    pub fn from_env() -> Result<Self> {
        let max_events_per_run = env_usize(
            "IRONCREW_MAX_EVENTS",
            DEFAULT_MAX_EVENTS_PER_RUN,
            1,
            HARD_MAX_EVENTS_PER_RUN,
        )?;
        let max_bytes_per_run = env_usize(
            "IRONCREW_EVENT_REPLAY_MAX_BYTES",
            DEFAULT_MAX_BYTES_PER_RUN,
            MIN_EVENT_BYTES,
            HARD_MAX_BYTES_PER_RUN,
        )?;
        let max_event_bytes = env_usize(
            "IRONCREW_EVENT_MAX_BYTES",
            DEFAULT_MAX_EVENT_BYTES,
            MIN_EVENT_BYTES,
            HARD_MAX_EVENT_BYTES,
        )?;
        let retention_secs = env_u64(
            "IRONCREW_EVENT_JOURNAL_RETENTION_SECS",
            DEFAULT_JOURNAL_RETENTION_SECS,
            MIN_JOURNAL_RETENTION_SECS,
            MAX_JOURNAL_RETENTION_SECS,
        )?;
        let max_total_events = env_u64(
            "IRONCREW_EVENT_JOURNAL_MAX_TOTAL_EVENTS",
            DEFAULT_JOURNAL_MAX_TOTAL_EVENTS,
            1,
            HARD_JOURNAL_MAX_TOTAL_EVENTS,
        )?;
        let max_total_bytes = env_u64(
            "IRONCREW_EVENT_JOURNAL_MAX_TOTAL_BYTES",
            DEFAULT_JOURNAL_MAX_TOTAL_BYTES,
            MIN_EVENT_BYTES as u64,
            HARD_JOURNAL_MAX_TOTAL_BYTES,
        )?;
        let default_page_bytes = DEFAULT_JOURNAL_PAGE_MAX_BYTES.max(max_event_bytes);
        let page_max_bytes = env_usize(
            "IRONCREW_EVENT_JOURNAL_PAGE_MAX_BYTES",
            default_page_bytes,
            MIN_PAGE_BYTES,
            HARD_JOURNAL_PAGE_MAX_BYTES,
        )?;
        let poll_interval_ms = env_u64(
            "IRONCREW_EVENT_JOURNAL_POLL_INTERVAL_MS",
            DEFAULT_JOURNAL_POLL_INTERVAL_MS,
            MIN_JOURNAL_POLL_INTERVAL_MS,
            MAX_JOURNAL_POLL_INTERVAL_MS,
        )?;
        let read_timeout_ms = env_u64(
            "IRONCREW_EVENT_JOURNAL_READ_TIMEOUT_MS",
            DEFAULT_JOURNAL_READ_TIMEOUT_MS,
            MIN_JOURNAL_READ_TIMEOUT_MS,
            MAX_JOURNAL_READ_TIMEOUT_MS,
        )?;
        let write_timeout_ms = env_u64(
            "IRONCREW_EVENT_JOURNAL_WRITE_TIMEOUT_MS",
            DEFAULT_JOURNAL_WRITE_TIMEOUT_MS,
            MIN_JOURNAL_WRITE_TIMEOUT_MS,
            MAX_JOURNAL_WRITE_TIMEOUT_MS,
        )?;
        let prune_batch = env_usize(
            "IRONCREW_EVENT_JOURNAL_PRUNE_BATCH",
            DEFAULT_JOURNAL_PRUNE_BATCH,
            1,
            HARD_JOURNAL_PRUNE_BATCH,
        )?;

        let config = Self {
            max_events_per_run,
            max_bytes_per_run,
            max_event_bytes,
            retention: Duration::from_secs(retention_secs),
            max_total_events,
            max_total_bytes,
            page_max_events: max_events_per_run.min(DEFAULT_JOURNAL_PAGE_MAX_EVENTS),
            page_max_bytes,
            poll_interval: Duration::from_millis(poll_interval_ms),
            read_timeout: Duration::from_millis(read_timeout_ms),
            write_timeout: Duration::from_millis(write_timeout_ms),
            prune_batch,
        };
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        validate_usize_range(
            "max_events_per_run",
            self.max_events_per_run,
            1,
            HARD_MAX_EVENTS_PER_RUN,
        )?;
        validate_usize_range(
            "max_bytes_per_run",
            self.max_bytes_per_run,
            MIN_EVENT_BYTES,
            HARD_MAX_BYTES_PER_RUN,
        )?;
        validate_usize_range(
            "max_event_bytes",
            self.max_event_bytes,
            MIN_EVENT_BYTES,
            HARD_MAX_EVENT_BYTES,
        )?;
        validate_duration_secs(
            "retention",
            self.retention,
            MIN_JOURNAL_RETENTION_SECS,
            MAX_JOURNAL_RETENTION_SECS,
        )?;
        validate_u64_range(
            "max_total_events",
            self.max_total_events,
            1,
            HARD_JOURNAL_MAX_TOTAL_EVENTS,
        )?;
        validate_u64_range(
            "max_total_bytes",
            self.max_total_bytes,
            MIN_EVENT_BYTES as u64,
            HARD_JOURNAL_MAX_TOTAL_BYTES,
        )?;
        validate_usize_range(
            "page_max_events",
            self.page_max_events,
            1,
            DEFAULT_JOURNAL_PAGE_MAX_EVENTS,
        )?;
        validate_usize_range(
            "page_max_bytes",
            self.page_max_bytes,
            MIN_PAGE_BYTES,
            HARD_JOURNAL_PAGE_MAX_BYTES,
        )?;
        validate_duration_millis(
            "poll_interval",
            self.poll_interval,
            MIN_JOURNAL_POLL_INTERVAL_MS,
            MAX_JOURNAL_POLL_INTERVAL_MS,
        )?;
        validate_duration_millis(
            "read_timeout",
            self.read_timeout,
            MIN_JOURNAL_READ_TIMEOUT_MS,
            MAX_JOURNAL_READ_TIMEOUT_MS,
        )?;
        validate_duration_millis(
            "write_timeout",
            self.write_timeout,
            MIN_JOURNAL_WRITE_TIMEOUT_MS,
            MAX_JOURNAL_WRITE_TIMEOUT_MS,
        )?;
        if RunEventWriteTiming::checked(self.write_timeout).is_none() {
            return Err(config_error("write_timeout timing arithmetic overflow"));
        }
        validate_usize_range("prune_batch", self.prune_batch, 1, HARD_JOURNAL_PRUNE_BATCH)?;

        if self.max_event_bytes > self.max_bytes_per_run {
            return Err(config_error(
                "max_event_bytes cannot exceed max_bytes_per_run",
            ));
        }
        if self.max_event_bytes > self.page_max_bytes {
            return Err(config_error(
                "page_max_bytes must be at least max_event_bytes",
            ));
        }
        if self.max_events_per_run as u64 > self.max_total_events {
            return Err(config_error(
                "max_events_per_run cannot exceed max_total_events",
            ));
        }
        if self.max_bytes_per_run as u64 > self.max_total_bytes {
            return Err(config_error(
                "max_bytes_per_run cannot exceed max_total_bytes",
            ));
        }
        if self.page_max_events > self.max_events_per_run {
            return Err(config_error(
                "page_max_events cannot exceed max_events_per_run",
            ));
        }
        if self.prune_batch as u64 > self.max_total_events {
            return Err(config_error("prune_batch cannot exceed max_total_events"));
        }
        Ok(())
    }
}

/// Parsed SSE cursor. Fields are private so every instance has a valid run id
/// and a non-zero sequence.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RunEventCursor {
    run_id: String,
    sequence: u64,
}

impl RunEventCursor {
    pub fn new(
        run_id: impl Into<String>,
        sequence: u64,
    ) -> std::result::Result<Self, RunEventCursorError> {
        let run_id = run_id.into();
        validate_cursor_run_id(&run_id)?;
        if sequence == 0 {
            return Err(RunEventCursorError::ZeroSequence);
        }
        Ok(Self { run_id, sequence })
    }

    pub fn parse(value: &str) -> std::result::Result<Self, RunEventCursorError> {
        if value.len() > MAX_RUN_EVENT_CURSOR_BYTES {
            return Err(RunEventCursorError::TooLong {
                max_bytes: MAX_RUN_EVENT_CURSOR_BYTES,
            });
        }
        let (run_id, raw_sequence) = value
            .split_once(':')
            .ok_or(RunEventCursorError::Malformed)?;
        if run_id.contains(':') || raw_sequence.contains(':') || raw_sequence.is_empty() {
            return Err(RunEventCursorError::Malformed);
        }
        validate_cursor_run_id(run_id)?;
        if !raw_sequence.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(RunEventCursorError::InvalidSequence);
        }
        if raw_sequence.len() > 1 && raw_sequence.starts_with('0') {
            return Err(RunEventCursorError::NonCanonicalSequence);
        }
        let sequence = raw_sequence
            .parse::<u64>()
            .map_err(|_| RunEventCursorError::SequenceOverflow)?;
        Self::new(run_id, sequence)
    }

    pub fn parse_for_run(
        value: &str,
        expected_run_id: &str,
    ) -> std::result::Result<Self, RunEventCursorError> {
        validate_cursor_run_id(expected_run_id)?;
        let cursor = Self::parse(value)?;
        if cursor.run_id != expected_run_id {
            return Err(RunEventCursorError::CrossRun);
        }
        Ok(cursor)
    }

    pub fn run_id(&self) -> &str {
        &self.run_id
    }

    pub fn sequence(&self) -> u64 {
        self.sequence
    }

    /// Check that this cursor is replayable within current retained bounds.
    /// A cursor exactly at `dropped_through` is valid because all later events
    /// remain available; an older cursor would silently miss a prefix.
    pub fn validate_against(
        &self,
        bounds: &RunEventBounds,
    ) -> std::result::Result<(), RunEventCursorError> {
        if self.sequence > bounds.latest_sequence {
            return Err(RunEventCursorError::Ahead {
                latest_sequence: bounds.latest_sequence,
            });
        }
        if self.sequence < bounds.dropped_through {
            return Err(RunEventCursorError::Expired {
                dropped_through: bounds.dropped_through,
            });
        }
        Ok(())
    }
}

impl fmt::Display for RunEventCursor {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}:{}", self.run_id, self.sequence)
    }
}

impl FromStr for RunEventCursor {
    type Err = RunEventCursorError;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        Self::parse(value)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RunEventCursorError {
    #[error("run-event cursor exceeds the {max_bytes}-byte limit")]
    TooLong { max_bytes: usize },
    #[error("run-event cursor must contain valid ASCII")]
    NonAscii,
    #[error("run-event cursor must use '<run_id>:<sequence>'")]
    Malformed,
    #[error("run-event cursor contains an invalid run id")]
    InvalidRunId,
    #[error("run-event cursor sequence must contain only decimal digits")]
    InvalidSequence,
    #[error("run-event cursor sequence must not contain leading zeroes")]
    NonCanonicalSequence,
    #[error("run-event cursor sequence exceeds u64")]
    SequenceOverflow,
    #[error("run-event cursor sequence must be greater than zero")]
    ZeroSequence,
    #[error("run-event cursor belongs to a different run")]
    CrossRun,
    #[error("run-event cursor is ahead of the stream (latest sequence {latest_sequence})")]
    Ahead { latest_sequence: u64 },
    #[error("run-event cursor has expired (events through {dropped_through} were pruned)")]
    Expired { dropped_through: u64 },
}

/// Event waiting to be appended by a bounded writer.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct RunEventAppendEntry {
    pub sequence: u64,
    pub event_type: String,
    pub payload: serde_json::Value,
    pub payload_bytes: usize,
}

impl fmt::Debug for RunEventAppendEntry {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RunEventAppendEntry")
            .field("sequence", &self.sequence)
            .field("event_type", &self.event_type)
            .field("payload", &"<redacted>")
            .field("payload_bytes", &self.payload_bytes)
            .finish()
    }
}

impl RunEventAppendEntry {
    pub fn new(
        sequence: u64,
        event_type: impl Into<String>,
        payload: serde_json::Value,
        max_event_bytes: usize,
    ) -> Result<Self> {
        let payload_bytes = serialized_json_size(&payload)?;
        let entry = Self {
            sequence,
            event_type: event_type.into(),
            payload,
            payload_bytes,
        };
        entry.validate(max_event_bytes)?;
        Ok(entry)
    }

    pub fn validate(&self, max_event_bytes: usize) -> Result<()> {
        validate_sequence(self.sequence)?;
        validate_event_type(&self.event_type)?;
        validate_payload_event_type(&self.payload, &self.event_type)?;
        let actual_bytes = serialized_json_size(&self.payload)?;
        if self.payload_bytes != actual_bytes {
            return Err(IronCrewError::Validation(
                "Run-event payload byte count does not match its serialized size".into(),
            ));
        }
        if actual_bytes > max_event_bytes {
            return Err(IronCrewError::Validation(format!(
                "Run-event payload exceeds the {max_event_bytes}-byte limit"
            )));
        }
        Ok(())
    }
}

/// One owner-fenced append operation. Entries must be strictly increasing; a
/// deliberate gap may be represented by a `journal_gap` event at a later
/// sequence rather than silently renumbering subsequent events.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunEventAppendBatch {
    pub run_id: String,
    pub flow: String,
    pub owner_instance_id: String,
    pub entries: Vec<RunEventAppendEntry>,
}

impl RunEventAppendBatch {
    pub fn validate(&self, config: &RunEventJournalConfig) -> Result<()> {
        config.validate()?;
        validate_run_id(&self.run_id)?;
        validate_flow(&self.flow)?;
        validate_owner_instance_id(&self.owner_instance_id)?;
        if self.entries.is_empty() {
            return Err(IronCrewError::Validation(
                "Run-event append batch must contain at least one event".into(),
            ));
        }
        if self.entries.len() > config.max_events_per_run {
            return Err(IronCrewError::Validation(format!(
                "Run-event append batch exceeds the {}-event per-run limit",
                config.max_events_per_run
            )));
        }

        let mut previous = 0u64;
        let mut total_bytes = 0usize;
        for entry in &self.entries {
            entry.validate(config.max_event_bytes)?;
            if entry.sequence <= previous {
                return Err(IronCrewError::Validation(
                    "Run-event append sequences must be strictly increasing".into(),
                ));
            }
            previous = entry.sequence;
            total_bytes = total_bytes
                .checked_add(entry.payload_bytes)
                .ok_or_else(|| IronCrewError::Validation("Run-event batch size overflow".into()))?;
        }
        if total_bytes > config.max_bytes_per_run {
            return Err(IronCrewError::Validation(format!(
                "Run-event append batch exceeds the {}-byte per-run limit",
                config.max_bytes_per_run
            )));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunEventGapReason {
    WriterBackpressure,
    Retention,
    GlobalCapacity,
    OwnerLost,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunEventGap {
    pub first_sequence: u64,
    pub last_sequence: u64,
    pub reason: RunEventGapReason,
}

impl RunEventGap {
    pub fn validate(&self) -> Result<()> {
        validate_sequence(self.first_sequence)?;
        validate_sequence(self.last_sequence)?;
        if self.first_sequence > self.last_sequence {
            return Err(IronCrewError::Validation(
                "Run-event gap starts after it ends".into(),
            ));
        }
        Ok(())
    }

    pub fn event_count(&self) -> u64 {
        // Both endpoints are non-zero and ordered after validation. Saturation
        // keeps this helper total even if a deserialized value was not checked.
        self.last_sequence
            .saturating_sub(self.first_sequence)
            .saturating_add(1)
    }
}

/// Exact retained-window metadata required to classify an SSE cursor without
/// loading event payloads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunEventBounds {
    pub earliest_retained_sequence: Option<u64>,
    /// Highest sequence allocated, including explicit gap markers.
    pub latest_sequence: u64,
    /// Highest sequence removed from the retained prefix. A cursor equal to
    /// this boundary can continue at the next retained sequence.
    pub dropped_through: u64,
    pub retained_events: u64,
    pub retained_bytes: u64,
    /// False means a bounded writer gap or owner loss may have omitted events.
    pub journal_complete: bool,
}

impl RunEventBounds {
    pub fn empty() -> Self {
        Self {
            earliest_retained_sequence: None,
            latest_sequence: 0,
            dropped_through: 0,
            retained_events: 0,
            retained_bytes: 0,
            journal_complete: true,
        }
    }

    pub fn validate(&self) -> Result<()> {
        if self.dropped_through > self.latest_sequence {
            return Err(IronCrewError::Validation(
                "Run-event dropped boundary exceeds the latest sequence".into(),
            ));
        }
        match self.earliest_retained_sequence {
            Some(earliest) => {
                validate_sequence(earliest)?;
                if self.retained_events == 0 {
                    return Err(IronCrewError::Validation(
                        "Run-event bounds have an earliest event but zero retained events".into(),
                    ));
                }
                if earliest <= self.dropped_through || earliest > self.latest_sequence {
                    return Err(IronCrewError::Validation(
                        "Run-event earliest retained sequence is outside its bounds".into(),
                    ));
                }
            }
            None => {
                if self.retained_events != 0 || self.retained_bytes != 0 {
                    return Err(IronCrewError::Validation(
                        "Run-event empty bounds retain count or bytes".into(),
                    ));
                }
            }
        }
        if self.retained_events > 0 && self.retained_bytes == 0 {
            return Err(IronCrewError::Validation(
                "Run-event retained entries must account for payload bytes".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunEventAppendOutcome {
    pub appended_events: u64,
    pub duplicate_events: u64,
    pub evicted_events: u64,
    pub evicted_bytes: u64,
    pub eviction_gap: Option<RunEventGap>,
    pub bounds: RunEventBounds,
}

impl RunEventAppendOutcome {
    pub fn validate(&self) -> Result<()> {
        self.bounds.validate()?;
        if let Some(gap) = &self.eviction_gap {
            gap.validate()?;
            if gap.last_sequence > self.bounds.dropped_through {
                return Err(IronCrewError::Validation(
                    "Run-event eviction gap exceeds the dropped boundary".into(),
                ));
            }
        }
        Ok(())
    }
}

/// One durable row returned to an SSE page. Debug output deliberately redacts
/// the payload because model output, logs, and prompts may contain secrets.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct RunEventEntry {
    pub sequence: u64,
    pub event_type: String,
    pub payload: serde_json::Value,
    pub payload_bytes: usize,
    pub created_at: String,
}

impl fmt::Debug for RunEventEntry {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RunEventEntry")
            .field("sequence", &self.sequence)
            .field("event_type", &self.event_type)
            .field("payload", &"<redacted>")
            .field("payload_bytes", &self.payload_bytes)
            .field("created_at", &self.created_at)
            .finish()
    }
}

impl RunEventEntry {
    pub fn validate(&self, config: &RunEventJournalConfig) -> Result<()> {
        RunEventAppendEntry {
            sequence: self.sequence,
            event_type: self.event_type.clone(),
            payload: self.payload.clone(),
            payload_bytes: self.payload_bytes,
        }
        .validate(config.max_event_bytes)?;
        chrono::DateTime::parse_from_rfc3339(&self.created_at).map_err(|error| {
            IronCrewError::Validation(format!("Run-event created_at must be RFC3339: {error}"))
        })?;
        Ok(())
    }
}

/// Terminal run metadata returned alongside a page. `event_sequence` is absent
/// while the terminal journal row is still pending or when its bounded writer
/// failed permanently; the authoritative run record still lets clients close
/// with an explicitly incomplete synthetic terminal event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunEventTerminalState {
    pub status: RunStatus,
    pub duration_ms: u64,
    pub total_tokens: u32,
    pub event_sequence: Option<u64>,
}

impl RunEventTerminalState {
    pub fn validate(&self) -> Result<()> {
        if !self.status.is_terminal() {
            return Err(IronCrewError::Validation(
                "Run-event terminal state must contain a terminal run status".into(),
            ));
        }
        if let Some(sequence) = self.event_sequence {
            validate_sequence(sequence)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunEventPage {
    pub run_id: String,
    /// Sequence requested by the caller; zero means the retained beginning.
    pub after_sequence: u64,
    pub events: Vec<RunEventEntry>,
    pub bounds: RunEventBounds,
    pub gap: Option<RunEventGap>,
    pub terminal: Option<RunEventTerminalState>,
}

impl RunEventPage {
    pub fn validate(&self, config: &RunEventJournalConfig) -> Result<()> {
        config.validate()?;
        validate_run_id(&self.run_id)?;
        self.bounds.validate()?;
        if self.after_sequence > self.bounds.latest_sequence {
            return Err(IronCrewError::Validation(
                "Run-event page starts ahead of the stream".into(),
            ));
        }
        if self.events.len() > config.page_max_events {
            return Err(IronCrewError::Validation(format!(
                "Run-event page exceeds the {}-event page limit",
                config.page_max_events
            )));
        }
        let mut previous = self.after_sequence;
        let mut total_bytes = 0usize;
        for event in &self.events {
            event.validate(config)?;
            if event.sequence != previous.saturating_add(1)
                || event.sequence > self.bounds.latest_sequence
            {
                return Err(IronCrewError::Validation(
                    "Run-event page must contain one contiguous sequence within bounds".into(),
                ));
            }
            previous = event.sequence;
            total_bytes = total_bytes
                .checked_add(event.payload_bytes)
                .ok_or_else(|| IronCrewError::Validation("Run-event page size overflow".into()))?;
        }
        if total_bytes > config.page_max_bytes {
            return Err(IronCrewError::Validation(format!(
                "Run-event page exceeds the {}-byte page limit",
                config.page_max_bytes
            )));
        }
        if let Some(gap) = &self.gap {
            gap.validate()?;
            if !self.events.is_empty()
                || gap.first_sequence != self.after_sequence.saturating_add(1)
            {
                return Err(IronCrewError::Validation(
                    "Run-event gap pages must contain no events and start immediately after the requested cursor"
                        .into(),
                ));
            }
            if gap.last_sequence > self.bounds.latest_sequence {
                return Err(IronCrewError::Validation(
                    "Run-event page gap exceeds the latest sequence".into(),
                ));
            }
        }
        if self.events.is_empty()
            && self.gap.is_none()
            && self.after_sequence < self.bounds.latest_sequence
        {
            return Err(IronCrewError::Validation(
                "Run-event page must expose the next retained event or sequence gap".into(),
            ));
        }
        if let Some(terminal) = &self.terminal {
            terminal.validate()?;
            if terminal
                .event_sequence
                .is_some_and(|sequence| sequence > self.bounds.latest_sequence)
            {
                return Err(IronCrewError::Validation(
                    "Run-event terminal sequence exceeds page bounds".into(),
                ));
            }
        }
        Ok(())
    }
}

fn validate_cursor_run_id(run_id: &str) -> std::result::Result<(), RunEventCursorError> {
    validate_run_id(run_id).map_err(|_| RunEventCursorError::InvalidRunId)
}

fn validate_sequence(sequence: u64) -> Result<()> {
    if sequence == 0 {
        return Err(IronCrewError::Validation(
            "Run-event sequence must be greater than zero".into(),
        ));
    }
    Ok(())
}

fn validate_event_type(event_type: &str) -> Result<()> {
    if event_type.is_empty()
        || event_type.len() > MAX_EVENT_TYPE_BYTES
        || !event_type
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
    {
        return Err(IronCrewError::Validation(format!(
            "Run-event type must be 1-{MAX_EVENT_TYPE_BYTES} lowercase ASCII alphanumeric/underscore bytes"
        )));
    }
    Ok(())
}

fn validate_payload_event_type(payload: &serde_json::Value, event_type: &str) -> Result<()> {
    if payload.get("event").and_then(serde_json::Value::as_str) != Some(event_type) {
        return Err(IronCrewError::Validation(
            "Run-event payload tag does not match its event type".into(),
        ));
    }
    Ok(())
}

fn validate_flow(flow: &str) -> Result<()> {
    if flow.is_empty()
        || flow.len() > MAX_FLOW_BYTES
        || flow.chars().any(char::is_control)
        || matches!(flow, "." | "..")
    {
        return Err(IronCrewError::Validation(
            "Run-event flow must be 1-255 non-control UTF-8 bytes and not '.' or '..'".into(),
        ));
    }
    Ok(())
}

fn validate_owner_instance_id(owner: &str) -> Result<()> {
    if owner.is_empty()
        || owner.len() > MAX_OWNER_INSTANCE_ID_BYTES
        || !owner
            .bytes()
            .all(|byte| byte.is_ascii() && !byte.is_ascii_control())
    {
        return Err(IronCrewError::Validation(
            "Run-event owner instance id must be 1-255 printable ASCII bytes".into(),
        ));
    }
    Ok(())
}

fn serialized_json_size(value: &serde_json::Value) -> Result<usize> {
    struct Counter(usize);

    impl std::io::Write for Counter {
        fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
            self.0 = self.0.saturating_add(buffer.len());
            Ok(buffer.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    let mut counter = Counter(0);
    serde_json::to_writer(&mut counter, value).map_err(|error| {
        IronCrewError::Validation(format!("Failed to size run-event payload: {error}"))
    })?;
    Ok(counter.0)
}

fn env_u64(name: &str, default: u64, min: u64, max: u64) -> Result<u64> {
    let value = match std::env::var(name) {
        Ok(raw) => raw.parse::<u64>().map_err(|_| {
            IronCrewError::Validation(format!("{name} must be an integer between {min} and {max}"))
        })?,
        Err(std::env::VarError::NotPresent) => default,
        Err(std::env::VarError::NotUnicode(_)) => {
            return Err(IronCrewError::Validation(format!(
                "{name} must contain valid UTF-8"
            )));
        }
    };
    if !(min..=max).contains(&value) {
        return Err(IronCrewError::Validation(format!(
            "{name} must be between {min} and {max}"
        )));
    }
    Ok(value)
}

fn env_usize(name: &str, default: usize, min: usize, max: usize) -> Result<usize> {
    let value = env_u64(name, default as u64, min as u64, max as u64)?;
    usize::try_from(value)
        .map_err(|_| IronCrewError::Validation(format!("{name} does not fit this platform")))
}

fn config_error(message: &str) -> IronCrewError {
    IronCrewError::Validation(format!(
        "Invalid run-event journal configuration: {message}"
    ))
}

fn validate_usize_range(label: &str, value: usize, min: usize, max: usize) -> Result<()> {
    if !(min..=max).contains(&value) {
        return Err(config_error(&format!(
            "{label} must be between {min} and {max}"
        )));
    }
    Ok(())
}

fn validate_u64_range(label: &str, value: u64, min: u64, max: u64) -> Result<()> {
    if !(min..=max).contains(&value) {
        return Err(config_error(&format!(
            "{label} must be between {min} and {max}"
        )));
    }
    Ok(())
}

fn validate_duration_secs(label: &str, value: Duration, min: u64, max: u64) -> Result<()> {
    if value.subsec_nanos() != 0 {
        return Err(config_error(&format!("{label} must use whole seconds")));
    }
    validate_u64_range(label, value.as_secs(), min, max)
}

fn validate_duration_millis(label: &str, value: Duration, min: u64, max: u64) -> Result<()> {
    let millis = value.as_millis();
    if millis > u64::MAX as u128 || Duration::from_millis(millis as u64) != value {
        return Err(config_error(&format!(
            "{label} must use whole milliseconds"
        )));
    }
    validate_u64_range(label, millis as u64, min, max)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn bounds() -> RunEventBounds {
        RunEventBounds {
            earliest_retained_sequence: Some(6),
            latest_sequence: 10,
            dropped_through: 5,
            retained_events: 5,
            retained_bytes: 500,
            journal_complete: true,
        }
    }

    fn event(sequence: u64, event_type: &str) -> RunEventAppendEntry {
        RunEventAppendEntry::new(
            sequence,
            event_type,
            json!({"event": event_type, "data": {"value": sequence}}),
            DEFAULT_MAX_EVENT_BYTES,
        )
        .unwrap()
    }

    #[test]
    fn cursor_round_trips_in_canonical_form() {
        let cursor = RunEventCursor::new("run-123", 42).unwrap();
        assert_eq!(cursor.to_string(), "run-123:42");
        assert_eq!(
            cursor.to_string().parse::<RunEventCursor>().unwrap(),
            cursor
        );
        assert_eq!(cursor.run_id(), "run-123");
        assert_eq!(cursor.sequence(), 42);
    }

    #[test]
    fn cursor_rejects_invalid_shapes_and_sequences() {
        for value in [
            "",
            "run-1",
            ":1",
            "run-1:",
            "run-1:0",
            "run-1:01",
            "run-1:-1",
            "run-1:1.0",
            "run-1:1:2",
            "../run:1",
        ] {
            assert!(RunEventCursor::parse(value).is_err(), "accepted {value:?}");
        }
    }

    #[test]
    fn cursor_rejects_oversized_input_before_parsing() {
        let value = "x".repeat(MAX_RUN_EVENT_CURSOR_BYTES + 1);
        assert_eq!(
            RunEventCursor::parse(&value),
            Err(RunEventCursorError::TooLong {
                max_bytes: MAX_RUN_EVENT_CURSOR_BYTES
            })
        );
    }

    #[test]
    fn cursor_rejects_cross_run_use() {
        assert_eq!(
            RunEventCursor::parse_for_run("run-a:2", "run-b"),
            Err(RunEventCursorError::CrossRun)
        );
    }

    #[test]
    fn cursor_distinguishes_u64_overflow() {
        assert_eq!(
            RunEventCursor::parse("run-a:18446744073709551616"),
            Err(RunEventCursorError::SequenceOverflow)
        );
    }

    #[test]
    fn cursor_rejects_ahead_and_expired_positions() {
        let ahead = RunEventCursor::new("run-a", 11).unwrap();
        assert_eq!(
            ahead.validate_against(&bounds()),
            Err(RunEventCursorError::Ahead {
                latest_sequence: 10
            })
        );

        let expired = RunEventCursor::new("run-a", 4).unwrap();
        assert_eq!(
            expired.validate_against(&bounds()),
            Err(RunEventCursorError::Expired { dropped_through: 5 })
        );

        RunEventCursor::new("run-a", 5)
            .unwrap()
            .validate_against(&bounds())
            .unwrap();
    }

    #[test]
    fn default_config_is_valid_and_derived_from_event_limits() {
        assert_eq!(
            EventJournalScope::default(),
            EventJournalScope::ProcessLocal
        );
        let config = RunEventJournalConfig::default();
        config.validate().unwrap();
        let _from_env: fn() -> Result<RunEventJournalConfig> = RunEventJournalConfig::from_env;
        assert_eq!(config.max_events_per_run, DEFAULT_MAX_EVENTS_PER_RUN);
        assert_eq!(config.max_bytes_per_run, DEFAULT_MAX_BYTES_PER_RUN);
        assert_eq!(config.max_event_bytes, DEFAULT_MAX_EVENT_BYTES);
        assert_eq!(config.page_max_events, 64);
        assert!(config.page_max_bytes >= config.max_event_bytes);
        assert_eq!(
            config.write_timeout,
            Duration::from_millis(DEFAULT_JOURNAL_WRITE_TIMEOUT_MS)
        );
    }

    #[test]
    fn config_rejects_out_of_range_and_inconsistent_limits() {
        let invalid_configs = [
            RunEventJournalConfig {
                max_events_per_run: 0,
                ..RunEventJournalConfig::default()
            },
            RunEventJournalConfig {
                retention: Duration::from_secs(MIN_JOURNAL_RETENTION_SECS - 1),
                ..RunEventJournalConfig::default()
            },
            RunEventJournalConfig {
                poll_interval: Duration::from_millis(MAX_JOURNAL_POLL_INTERVAL_MS + 1),
                ..RunEventJournalConfig::default()
            },
            RunEventJournalConfig {
                read_timeout: Duration::from_millis(MAX_JOURNAL_READ_TIMEOUT_MS + 1),
                ..RunEventJournalConfig::default()
            },
            RunEventJournalConfig {
                write_timeout: Duration::from_millis(MIN_JOURNAL_WRITE_TIMEOUT_MS - 1),
                ..RunEventJournalConfig::default()
            },
            RunEventJournalConfig {
                write_timeout: Duration::from_millis(MAX_JOURNAL_WRITE_TIMEOUT_MS + 1),
                ..RunEventJournalConfig::default()
            },
            RunEventJournalConfig {
                page_max_bytes: DEFAULT_MAX_EVENT_BYTES - 1,
                ..RunEventJournalConfig::default()
            },
            RunEventJournalConfig {
                max_total_events: DEFAULT_MAX_EVENTS_PER_RUN as u64 - 1,
                ..RunEventJournalConfig::default()
            },
            RunEventJournalConfig {
                max_total_bytes: DEFAULT_MAX_BYTES_PER_RUN as u64 - 1,
                ..RunEventJournalConfig::default()
            },
        ];

        for config in invalid_configs {
            assert!(config.validate().is_err());
        }
    }

    #[test]
    fn config_accepts_documented_hard_boundaries() {
        let config = RunEventJournalConfig {
            max_events_per_run: HARD_MAX_EVENTS_PER_RUN,
            max_bytes_per_run: HARD_MAX_BYTES_PER_RUN,
            max_event_bytes: HARD_MAX_EVENT_BYTES,
            retention: Duration::from_secs(MAX_JOURNAL_RETENTION_SECS),
            max_total_events: HARD_JOURNAL_MAX_TOTAL_EVENTS,
            max_total_bytes: HARD_JOURNAL_MAX_TOTAL_BYTES,
            page_max_events: DEFAULT_JOURNAL_PAGE_MAX_EVENTS,
            page_max_bytes: HARD_JOURNAL_PAGE_MAX_BYTES,
            poll_interval: Duration::from_millis(MAX_JOURNAL_POLL_INTERVAL_MS),
            read_timeout: Duration::from_millis(MAX_JOURNAL_READ_TIMEOUT_MS),
            write_timeout: Duration::from_millis(MAX_JOURNAL_WRITE_TIMEOUT_MS),
            prune_batch: HARD_JOURNAL_PRUNE_BATCH,
        };
        config.validate().unwrap();
    }

    #[test]
    fn entries_are_sized_and_debug_redacts_payloads() {
        let entry = event(1, "crew_started");
        entry.validate(DEFAULT_MAX_EVENT_BYTES).unwrap();
        let debug = format!("{entry:?}");
        assert!(debug.contains("<redacted>"));
        assert!(!debug.contains("value"));

        let mut forged = entry.clone();
        forged.payload_bytes += 1;
        assert!(forged.validate(DEFAULT_MAX_EVENT_BYTES).is_err());
    }

    #[test]
    fn append_batch_requires_strict_order_and_resource_bounds() {
        let config = RunEventJournalConfig::default();
        let valid = RunEventAppendBatch {
            run_id: "run-a".into(),
            flow: "flow-a".into(),
            owner_instance_id: "pod-a".into(),
            entries: vec![event(1, "crew_started"), event(3, "journal_gap")],
        };
        valid.validate(&config).unwrap();

        let mut unordered = valid;
        unordered.entries.swap(0, 1);
        assert!(unordered.validate(&config).is_err());
    }

    #[test]
    fn bounds_gap_terminal_and_page_validation_fail_closed() {
        RunEventBounds::empty().validate().unwrap();
        bounds().validate().unwrap();
        let gap = RunEventGap {
            first_sequence: 2,
            last_sequence: 5,
            reason: RunEventGapReason::Retention,
        };
        gap.validate().unwrap();
        assert_eq!(gap.event_count(), 4);
        RunEventAppendOutcome {
            appended_events: 1,
            duplicate_events: 0,
            evicted_events: gap.event_count(),
            evicted_bytes: 400,
            eviction_gap: Some(gap),
            bounds: bounds(),
        }
        .validate()
        .unwrap();
        RunEventTerminalState {
            status: RunStatus::Success,
            duration_ms: 10,
            total_tokens: 20,
            event_sequence: Some(10),
        }
        .validate()
        .unwrap();

        let config = RunEventJournalConfig::default();
        let append = event(6, "crew_started");
        let page = RunEventPage {
            run_id: "run-a".into(),
            after_sequence: 5,
            events: vec![RunEventEntry {
                sequence: append.sequence,
                event_type: append.event_type,
                payload: append.payload,
                payload_bytes: append.payload_bytes,
                created_at: "2026-07-19T12:00:00Z".into(),
            }],
            bounds: bounds(),
            gap: None,
            terminal: Some(RunEventTerminalState {
                status: RunStatus::Success,
                duration_ms: 10,
                total_tokens: 20,
                event_sequence: Some(10),
            }),
        };
        page.validate(&config).unwrap();

        let mut ahead = page;
        ahead.after_sequence = 11;
        assert!(ahead.validate(&config).is_err());
    }

    #[test]
    fn page_validation_requires_one_contiguous_shape() {
        let config = RunEventJournalConfig::default();
        let entry = |sequence, event_type| {
            let append = event(sequence, event_type);
            RunEventEntry {
                sequence: append.sequence,
                event_type: append.event_type,
                payload: append.payload,
                payload_bytes: append.payload_bytes,
                created_at: "2026-07-19T12:00:00Z".into(),
            }
        };

        let gap_only = RunEventPage {
            run_id: "run-a".into(),
            after_sequence: 5,
            events: Vec::new(),
            bounds: bounds(),
            gap: Some(RunEventGap {
                first_sequence: 6,
                last_sequence: 7,
                reason: RunEventGapReason::WriterBackpressure,
            }),
            terminal: None,
        };
        gap_only.validate(&config).unwrap();

        let mut mixed = gap_only.clone();
        mixed.events.push(entry(6, "crew_started"));
        assert!(mixed.validate(&config).is_err());

        let discontinuous = RunEventPage {
            run_id: "run-a".into(),
            after_sequence: 5,
            events: vec![entry(6, "crew_started"), entry(8, "task_assigned")],
            bounds: bounds(),
            gap: None,
            terminal: None,
        };
        assert!(discontinuous.validate(&config).is_err());

        let hidden_gap = RunEventPage {
            run_id: "run-a".into(),
            after_sequence: 5,
            events: Vec::new(),
            bounds: bounds(),
            gap: None,
            terminal: None,
        };
        assert!(hidden_gap.validate(&config).is_err());
    }
}
