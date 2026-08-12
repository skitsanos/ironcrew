//! Process-global, fixed-cardinality execution and storage metrics.
//!
//! All recording operations are synchronous relaxed atomics. Labels are
//! represented by closed enums so request, provider, tool, flow, and error
//! strings cannot reach the Prometheus surface.

mod histogram;
mod labels;
mod prometheus;
mod state;

use std::sync::LazyLock;
use std::time::Duration;

pub use labels::{
    LeaseScope, ProviderFamily, ProviderOperation, ProviderOutcome, ReconciliationOutcome,
    RunOutcome, SseOutcome, SseScope, StoreOperation, TaskOutcome, TerminalOutcome, TerminalScope,
    TokenKind, ToolOutcome,
};

static METRICS: LazyLock<state::Metrics> = LazyLock::new(state::Metrics::default);

pub fn record_run_count(outcome: RunOutcome, count: usize) {
    METRICS.record_run_count(outcome, count);
}

pub fn record_run(outcome: RunOutcome, duration: Option<Duration>) {
    METRICS.record_run(outcome, duration);
}

pub fn record_task(outcome: TaskOutcome, duration: Duration) {
    METRICS.record_task(outcome, duration);
}

pub fn record_tool(outcome: ToolOutcome, duration: Duration) {
    METRICS.record_tool(outcome, duration);
}

pub fn record_provider(
    family: ProviderFamily,
    operation: ProviderOperation,
    outcome: ProviderOutcome,
    duration: Duration,
) {
    METRICS.record_provider(family, operation, outcome, duration);
}

pub fn record_provider_tokens(family: ProviderFamily, usage: &crate::llm::provider::TokenUsage) {
    METRICS.record_provider_tokens(
        family,
        [
            u64::from(usage.prompt_tokens),
            u64::from(usage.completion_tokens),
            u64::from(usage.cached_tokens),
        ],
    );
}

pub fn record_sse(scope: SseScope, outcome: SseOutcome) {
    METRICS.record_sse(scope, outcome);
}

pub fn record_lease_loss(scope: LeaseScope) {
    METRICS.record_lease_loss(scope);
}

pub fn record_reconciliation(outcome: ReconciliationOutcome, reconciled: usize) {
    METRICS.record_reconciliation(outcome, reconciled);
}

pub fn record_terminal_persistence(scope: TerminalScope, outcome: TerminalOutcome) {
    METRICS.record_terminal(scope, outcome);
}

pub fn record_store_failure(operation: StoreOperation) {
    METRICS.record_store_failure(operation);
}

pub(crate) fn record_store_error(
    operation: StoreOperation,
    error: &crate::utils::error::IronCrewError,
) {
    if store_error_is_failure(error) {
        record_store_failure(operation);
    }
}

fn store_error_is_failure(error: &crate::utils::error::IronCrewError) -> bool {
    !matches!(
        error,
        crate::utils::error::IronCrewError::Conflict(_)
            | crate::utils::error::IronCrewError::OwnerDraining { .. }
    )
}

pub fn append_prometheus(body: &mut String) {
    prometheus::append(body, &METRICS);
}

#[cfg(test)]
mod tests;
