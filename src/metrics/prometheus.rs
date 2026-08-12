use std::fmt::Write;

use super::histogram::{DURATION_BUCKET_LABELS, Histogram};
use super::state::Metrics;
use super::{
    LeaseScope, ProviderFamily, ProviderOperation, ProviderOutcome, ReconciliationOutcome,
    RunOutcome, SseOutcome, SseScope, StoreOperation, TaskOutcome, TerminalOutcome, TerminalScope,
    TokenKind, ToolOutcome,
};

pub(crate) fn append(body: &mut String, metrics: &Metrics) {
    append_outcome_durations(
        body,
        "ironcrew_runs_total",
        "ironcrew_run_duration_seconds",
        RunOutcome::ALL
            .iter()
            .map(|value| value.as_str())
            .zip(metrics.run_counts.iter().map(Metrics::counter))
            .zip(metrics.run_durations.iter()),
    );
    append_outcome_durations(
        body,
        "ironcrew_tasks_total",
        "ironcrew_task_duration_seconds",
        TaskOutcome::ALL
            .iter()
            .map(|value| value.as_str())
            .zip(metrics.task_counts.iter().map(Metrics::counter))
            .zip(metrics.task_durations.iter()),
    );
    append_outcome_durations(
        body,
        "ironcrew_tool_calls_total",
        "ironcrew_tool_call_duration_seconds",
        ToolOutcome::ALL
            .iter()
            .map(|value| value.as_str())
            .zip(metrics.tool_counts.iter().map(Metrics::counter))
            .zip(metrics.tool_durations.iter()),
    );

    writeln!(body, "# TYPE ironcrew_provider_requests_total counter").unwrap();
    writeln!(
        body,
        "# TYPE ironcrew_provider_request_duration_seconds histogram"
    )
    .unwrap();
    for &family in ProviderFamily::ALL {
        for &operation in ProviderOperation::ALL {
            for &outcome in ProviderOutcome::ALL {
                let labels = format!(
                    "provider=\"{}\",operation=\"{}\",outcome=\"{}\"",
                    family.as_str(),
                    operation.as_str(),
                    outcome.as_str()
                );
                let count = Metrics::counter(
                    &metrics.provider_counts[family.index()][operation.index()][outcome.index()],
                );
                writeln!(body, "ironcrew_provider_requests_total{{{labels}}} {count}").unwrap();
                append_histogram(
                    body,
                    "ironcrew_provider_request_duration_seconds",
                    &labels,
                    &metrics.provider_durations[family.index()][operation.index()][outcome.index()],
                );
            }
        }
    }

    writeln!(body, "# TYPE ironcrew_provider_tokens_total counter").unwrap();
    for &family in ProviderFamily::ALL {
        for &kind in TokenKind::ALL {
            let value = Metrics::counter(&metrics.provider_tokens[family.index()][kind.index()]);
            writeln!(
                body,
                "ironcrew_provider_tokens_total{{provider=\"{}\",type=\"{}\"}} {value}",
                family.as_str(),
                kind.as_str()
            )
            .unwrap();
        }
    }

    writeln!(body, "# TYPE ironcrew_sse_connections_total counter").unwrap();
    for &scope in SseScope::ALL {
        for &outcome in SseOutcome::ALL {
            let value = Metrics::counter(&metrics.sse_counts[scope.index()][outcome.index()]);
            writeln!(
                body,
                "ironcrew_sse_connections_total{{scope=\"{}\",outcome=\"{}\"}} {value}",
                scope.as_str(),
                outcome.as_str()
            )
            .unwrap();
        }
    }

    append_single_label_counters(
        body,
        "ironcrew_lease_losses_total",
        "scope",
        LeaseScope::ALL
            .iter()
            .map(|value| value.as_str())
            .zip(metrics.lease_losses.iter().map(Metrics::counter)),
    );
    append_single_label_counters(
        body,
        "ironcrew_reconciliation_cycles_total",
        "outcome",
        ReconciliationOutcome::ALL
            .iter()
            .map(|value| value.as_str())
            .zip(metrics.reconciliation_cycles.iter().map(Metrics::counter)),
    );
    writeln!(
        body,
        "# TYPE ironcrew_reconciliation_records_total counter\nironcrew_reconciliation_records_total {}",
        Metrics::counter(&metrics.reconciliation_records)
    )
    .unwrap();

    writeln!(body, "# TYPE ironcrew_terminal_persistence_total counter").unwrap();
    for &scope in TerminalScope::ALL {
        for &outcome in TerminalOutcome::ALL {
            let value = Metrics::counter(&metrics.terminal_counts[scope.index()][outcome.index()]);
            writeln!(
                body,
                "ironcrew_terminal_persistence_total{{scope=\"{}\",outcome=\"{}\"}} {value}",
                scope.as_str(),
                outcome.as_str()
            )
            .unwrap();
        }
    }
    append_single_label_counters(
        body,
        "ironcrew_store_failures_total",
        "operation",
        StoreOperation::ALL
            .iter()
            .map(|value| value.as_str())
            .zip(metrics.store_failures.iter().map(Metrics::counter)),
    );
}

fn append_outcome_durations<'a>(
    body: &mut String,
    counter_name: &str,
    histogram_name: &str,
    rows: impl Iterator<Item = ((&'a str, u64), &'a Histogram)>,
) {
    writeln!(body, "# TYPE {counter_name} counter").unwrap();
    writeln!(body, "# TYPE {histogram_name} histogram").unwrap();
    for ((outcome, count), histogram) in rows {
        writeln!(body, "{counter_name}{{outcome=\"{outcome}\"}} {count}").unwrap();
        append_histogram(
            body,
            histogram_name,
            &format!("outcome=\"{outcome}\""),
            histogram,
        );
    }
}

pub(super) fn append_histogram(body: &mut String, name: &str, labels: &str, histogram: &Histogram) {
    let snapshot = histogram.snapshot();
    let mut cumulative = 0u64;
    for (index, upper_bound) in DURATION_BUCKET_LABELS.iter().enumerate() {
        cumulative = cumulative.saturating_add(snapshot.buckets[index]);
        writeln!(
            body,
            "{name}_bucket{{{labels},le=\"{upper_bound}\"}} {cumulative}"
        )
        .unwrap();
    }
    let count = cumulative.saturating_add(snapshot.buckets[DURATION_BUCKET_LABELS.len()]);
    writeln!(body, "{name}_bucket{{{labels},le=\"+Inf\"}} {count}").unwrap();
    let micros = snapshot.sum_micros;
    writeln!(
        body,
        "{name}_sum{{{labels}}} {}.{:06}",
        micros / 1_000_000,
        micros % 1_000_000
    )
    .unwrap();
    writeln!(body, "{name}_count{{{labels}}} {count}").unwrap();
}

fn append_single_label_counters<'a>(
    body: &mut String,
    name: &str,
    label_name: &str,
    rows: impl Iterator<Item = (&'a str, u64)>,
) {
    writeln!(body, "# TYPE {name} counter").unwrap();
    for (label, value) in rows {
        writeln!(body, "{name}{{{label_name}=\"{label}\"}} {value}").unwrap();
    }
}
