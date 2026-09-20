use std::collections::BTreeMap;

use crate::engine::run_events::{RunEventAppendEntry, RunEventGap, RunEventGapReason};
use crate::engine::run_history::RunStatus;

#[derive(Debug, Clone)]
pub(in crate::engine::postgres_store) struct RunEventStateRow {
    pub(in crate::engine::postgres_store) flow: String,
    pub(in crate::engine::postgres_store) owner_instance_id: String,
    pub(in crate::engine::postgres_store) latest_sequence: u64,
    pub(in crate::engine::postgres_store) dropped_through: u64,
    pub(in crate::engine::postgres_store) retained_events: u64,
    pub(in crate::engine::postgres_store) retained_bytes: u64,
    pub(in crate::engine::postgres_store) journal_complete: bool,
    pub(in crate::engine::postgres_store) eviction_reason: Option<RunEventGapReason>,
    pub(in crate::engine::postgres_store) terminal_event_sequence: Option<u64>,
}

pub(in crate::engine::postgres_store) type RunEventStateDbRow = (
    String,
    String,
    i64,
    i64,
    i64,
    i64,
    bool,
    Option<String>,
    Option<i64>,
);

#[derive(Debug, Clone, Copy)]
pub(in crate::engine::postgres_store) struct RunEventUsageRow {
    pub(in crate::engine::postgres_store) retained_events: u64,
    pub(in crate::engine::postgres_store) retained_bytes: u64,
}

#[derive(Debug, Clone)]
pub(in crate::engine::postgres_store) struct RunEventDeleteCandidate {
    pub(in crate::engine::postgres_store) run_id: String,
    pub(in crate::engine::postgres_store) sequence: u64,
    pub(in crate::engine::postgres_store) payload_bytes: u64,
}

#[derive(Debug, Clone, Default)]
pub(in crate::engine::postgres_store) struct RunEventRunEviction {
    pub(in crate::engine::postgres_store) events: u64,
    pub(in crate::engine::postgres_store) bytes: u64,
    pub(in crate::engine::postgres_store) first_sequence: u64,
    pub(in crate::engine::postgres_store) last_sequence: u64,
    pub(in crate::engine::postgres_store) previous_dropped_through: u64,
    pub(in crate::engine::postgres_store) new_dropped_through: u64,
    pub(in crate::engine::postgres_store) reason: Option<RunEventGapReason>,
}

impl RunEventRunEviction {
    fn merge(&mut self, other: &Self) {
        if other.events == 0 {
            return;
        }
        if self.events == 0 {
            *self = other.clone();
            return;
        }
        self.events = self.events.saturating_add(other.events);
        self.bytes = self.bytes.saturating_add(other.bytes);
        self.first_sequence = self.first_sequence.min(other.first_sequence);
        self.last_sequence = self.last_sequence.max(other.last_sequence);
        self.previous_dropped_through = self
            .previous_dropped_through
            .min(other.previous_dropped_through);
        if other.new_dropped_through >= self.new_dropped_through {
            self.new_dropped_through = other.new_dropped_through;
            self.reason = other.reason;
        }
    }

    pub(in crate::engine::postgres_store) fn gap(&self) -> Option<RunEventGap> {
        let reason = self.reason?;
        if self.new_dropped_through <= self.previous_dropped_through {
            return None;
        }
        Some(RunEventGap {
            first_sequence: self.previous_dropped_through.saturating_add(1),
            last_sequence: self.new_dropped_through,
            reason,
        })
    }
}

#[derive(Debug, Default)]
pub(in crate::engine::postgres_store) struct RunEventPruneSummary {
    pub(in crate::engine::postgres_store) by_run: BTreeMap<String, RunEventRunEviction>,
}

impl RunEventPruneSummary {
    pub(in crate::engine::postgres_store) fn merge(&mut self, other: Self) {
        for (run_id, eviction) in other.by_run {
            self.by_run.entry(run_id).or_default().merge(&eviction);
        }
    }

    pub(in crate::engine::postgres_store) fn for_run(&self, run_id: &str) -> RunEventRunEviction {
        self.by_run.get(run_id).cloned().unwrap_or_default()
    }
}

pub(in crate::engine::postgres_store) struct RunEventAppendTarget {
    pub(in crate::engine::postgres_store) status: RunStatus,
    pub(in crate::engine::postgres_store) lease_active: bool,
    pub(in crate::engine::postgres_store) duration_ms: i64,
    pub(in crate::engine::postgres_store) total_tokens: i32,
}

pub(in crate::engine::postgres_store) struct AccountedRunEventBatch {
    pub(in crate::engine::postgres_store) entries: Vec<RunEventAppendEntry>,
    pub(in crate::engine::postgres_store) serialized_payloads: Vec<String>,
    pub(in crate::engine::postgres_store) event_count: u64,
    pub(in crate::engine::postgres_store) byte_count: u64,
}
