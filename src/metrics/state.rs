use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use super::histogram::{Histogram, saturating_add};
use super::{
    LeaseScope, ProviderFamily, ProviderOperation, ProviderOutcome, ReconciliationOutcome,
    RunOutcome, SseOutcome, SseScope, StoreOperation, TaskOutcome, TerminalOutcome, TerminalScope,
    TokenKind, ToolOutcome,
};

pub(crate) struct Metrics {
    pub(crate) run_counts: [AtomicU64; RunOutcome::COUNT],
    pub(crate) run_durations: [Histogram; RunOutcome::COUNT],
    pub(crate) task_counts: [AtomicU64; TaskOutcome::COUNT],
    pub(crate) task_durations: [Histogram; TaskOutcome::COUNT],
    pub(crate) tool_counts: [AtomicU64; ToolOutcome::COUNT],
    pub(crate) tool_durations: [Histogram; ToolOutcome::COUNT],
    pub(crate) provider_counts:
        [[[AtomicU64; ProviderOutcome::COUNT]; ProviderOperation::COUNT]; ProviderFamily::COUNT],
    pub(crate) provider_durations:
        [[[Histogram; ProviderOutcome::COUNT]; ProviderOperation::COUNT]; ProviderFamily::COUNT],
    pub(crate) provider_tokens: [[AtomicU64; TokenKind::COUNT]; ProviderFamily::COUNT],
    pub(crate) sse_counts: [[AtomicU64; SseOutcome::COUNT]; SseScope::COUNT],
    pub(crate) lease_losses: [AtomicU64; LeaseScope::COUNT],
    pub(crate) reconciliation_cycles: [AtomicU64; ReconciliationOutcome::COUNT],
    pub(crate) reconciliation_records: AtomicU64,
    pub(crate) terminal_counts: [[AtomicU64; TerminalOutcome::COUNT]; TerminalScope::COUNT],
    pub(crate) store_failures: [AtomicU64; StoreOperation::COUNT],
}

impl Default for Metrics {
    fn default() -> Self {
        Self {
            run_counts: std::array::from_fn(|_| AtomicU64::new(0)),
            run_durations: std::array::from_fn(|_| Histogram::default()),
            task_counts: std::array::from_fn(|_| AtomicU64::new(0)),
            task_durations: std::array::from_fn(|_| Histogram::default()),
            tool_counts: std::array::from_fn(|_| AtomicU64::new(0)),
            tool_durations: std::array::from_fn(|_| Histogram::default()),
            provider_counts: std::array::from_fn(|_| {
                std::array::from_fn(|_| std::array::from_fn(|_| AtomicU64::new(0)))
            }),
            provider_durations: std::array::from_fn(|_| {
                std::array::from_fn(|_| std::array::from_fn(|_| Histogram::default()))
            }),
            provider_tokens: std::array::from_fn(|_| std::array::from_fn(|_| AtomicU64::new(0))),
            sse_counts: std::array::from_fn(|_| std::array::from_fn(|_| AtomicU64::new(0))),
            lease_losses: std::array::from_fn(|_| AtomicU64::new(0)),
            reconciliation_cycles: std::array::from_fn(|_| AtomicU64::new(0)),
            reconciliation_records: AtomicU64::new(0),
            terminal_counts: std::array::from_fn(|_| std::array::from_fn(|_| AtomicU64::new(0))),
            store_failures: std::array::from_fn(|_| AtomicU64::new(0)),
        }
    }
}

impl Metrics {
    pub(crate) fn record_run_count(&self, outcome: RunOutcome, count: usize) {
        saturating_add(&self.run_counts[outcome.index()], count as u64);
    }

    pub(crate) fn record_run(&self, outcome: RunOutcome, duration: Option<Duration>) {
        self.record_run_count(outcome, 1);
        if let Some(duration) = duration {
            self.run_durations[outcome.index()].record(duration);
        }
    }

    pub(crate) fn record_task(&self, outcome: TaskOutcome, duration: Duration) {
        saturating_add(&self.task_counts[outcome.index()], 1);
        self.task_durations[outcome.index()].record(duration);
    }

    pub(crate) fn record_tool(&self, outcome: ToolOutcome, duration: Duration) {
        saturating_add(&self.tool_counts[outcome.index()], 1);
        self.tool_durations[outcome.index()].record(duration);
    }

    pub(crate) fn record_provider(
        &self,
        family: ProviderFamily,
        operation: ProviderOperation,
        outcome: ProviderOutcome,
        duration: Duration,
    ) {
        let counter = &self.provider_counts[family.index()][operation.index()][outcome.index()];
        saturating_add(counter, 1);
        self.provider_durations[family.index()][operation.index()][outcome.index()]
            .record(duration);
    }

    pub(crate) fn record_provider_tokens(&self, family: ProviderFamily, values: [u64; 3]) {
        for (kind, value) in TokenKind::ALL.iter().copied().zip(values) {
            saturating_add(&self.provider_tokens[family.index()][kind.index()], value);
        }
    }

    pub(crate) fn record_sse(&self, scope: SseScope, outcome: SseOutcome) {
        saturating_add(&self.sse_counts[scope.index()][outcome.index()], 1);
    }

    pub(crate) fn record_lease_loss(&self, scope: LeaseScope) {
        saturating_add(&self.lease_losses[scope.index()], 1);
    }

    pub(crate) fn record_reconciliation(&self, outcome: ReconciliationOutcome, reconciled: usize) {
        saturating_add(&self.reconciliation_cycles[outcome.index()], 1);
        saturating_add(&self.reconciliation_records, reconciled as u64);
    }

    pub(crate) fn record_terminal(&self, scope: TerminalScope, outcome: TerminalOutcome) {
        saturating_add(&self.terminal_counts[scope.index()][outcome.index()], 1);
    }

    pub(crate) fn record_store_failure(&self, operation: StoreOperation) {
        saturating_add(&self.store_failures[operation.index()], 1);
    }

    pub(crate) fn counter(counter: &AtomicU64) -> u64 {
        counter.load(Ordering::Relaxed)
    }
}
