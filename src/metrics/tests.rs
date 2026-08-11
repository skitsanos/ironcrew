use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};
use std::time::Duration;

use super::prometheus;
use super::state::Metrics;
use super::*;

fn render(metrics: &Metrics) -> String {
    let mut body = String::new();
    prometheus::append(&mut body, metrics);
    body
}

#[test]
fn labels_are_a_closed_vocabulary() {
    assert_eq!(
        ProviderFamily::ALL
            .iter()
            .map(|value| value.as_str())
            .collect::<Vec<_>>(),
        ["openai", "openai_responses", "anthropic", "other"]
    );
    assert_eq!(
        ProviderOperation::ALL
            .iter()
            .map(|value| value.as_str())
            .collect::<Vec<_>>(),
        ["chat", "chat_with_tools", "chat_stream"]
    );
    let body = render(&Metrics::default());
    assert_eq!(
        body.lines()
            .filter(|line| line.starts_with("ironcrew_provider_requests_total{"))
            .count(),
        ProviderFamily::COUNT * ProviderOperation::COUNT * ProviderOutcome::COUNT
    );
    for forbidden in [
        "run-7f9a-private-id",
        "customer-private-flow",
        "customer-private-task",
        "customer-private-tool",
        "https://private.example",
        "customer prompt contents",
        "super-secret-value",
        "raw provider error message",
    ] {
        assert!(!body.contains(forbidden), "unexpected label: {forbidden}");
    }
}

#[test]
fn provider_tokens_are_aggregated_by_fixed_family_and_kind() {
    let metrics = Metrics::default();
    metrics.record_provider_tokens(ProviderFamily::Anthropic, [10, 4, 3]);
    metrics.record_provider_tokens(ProviderFamily::Anthropic, [5, 2, 1]);
    let body = render(&metrics);
    assert!(
        body.contains(
            "ironcrew_provider_tokens_total{provider=\"anthropic\",type=\"prompt\"} 15\n"
        )
    );
    assert!(body.contains(
        "ironcrew_provider_tokens_total{provider=\"anthropic\",type=\"completion\"} 6\n"
    ));
    assert!(
        body.contains("ironcrew_provider_tokens_total{provider=\"anthropic\",type=\"cached\"} 4\n")
    );
}

#[test]
fn duration_histogram_buckets_are_cumulative_and_monotonic() {
    let metrics = Metrics::default();
    metrics.record_run(RunOutcome::Success, Some(Duration::from_millis(1)));
    metrics.record_run(RunOutcome::Success, Some(Duration::from_millis(20)));
    metrics.record_run(RunOutcome::Success, Some(Duration::from_secs(600)));
    let body = render(&metrics);
    let values = body
        .lines()
        .filter(|line| {
            line.starts_with("ironcrew_run_duration_seconds_bucket{outcome=\"success\",le=")
        })
        .map(|line| {
            line.rsplit_once(' ')
                .expect("bucket has a value")
                .1
                .parse::<u64>()
                .expect("bucket value is numeric")
        })
        .collect::<Vec<_>>();
    assert_eq!(
        values.len(),
        super::histogram::DURATION_BUCKETS_MICROS.len() + 1
    );
    assert!(values.windows(2).all(|pair| pair[0] <= pair[1]));
    assert_eq!(values.last(), Some(&3));
    assert!(body.contains("ironcrew_run_duration_seconds_count{outcome=\"success\"} 3\n"));
    assert!(body.contains("ironcrew_run_duration_seconds_sum{outcome=\"success\"} 600.021000\n"));
}

#[test]
fn concurrent_record_and_render_preserves_histogram_contract() {
    const WORKERS: usize = 4;
    const RECORDS: usize = 20_000;
    let histogram = Arc::new(super::histogram::Histogram::default());
    let barrier = Arc::new(Barrier::new(WORKERS + 1));
    let remaining = Arc::new(AtomicUsize::new(WORKERS));
    let workers = (0..WORKERS)
        .map(|worker| {
            let histogram = Arc::clone(&histogram);
            let barrier = Arc::clone(&barrier);
            let remaining = Arc::clone(&remaining);
            std::thread::spawn(move || {
                let durations = [
                    Duration::from_millis(1),
                    Duration::from_millis(20),
                    Duration::from_millis(750),
                    Duration::from_secs(600),
                ];
                barrier.wait();
                for index in 0..RECORDS {
                    histogram.record(durations[(worker + index) % durations.len()]);
                    if index % 16 == 0 {
                        std::thread::yield_now();
                    }
                }
                remaining.fetch_sub(1, Ordering::Release);
            })
        })
        .collect::<Vec<_>>();

    barrier.wait();
    let mut scrapes = 0usize;
    loop {
        let mut body = String::new();
        prometheus::append_histogram(
            &mut body,
            "test_duration_seconds",
            "scope=\"test\"",
            &histogram,
        );
        let buckets = body
            .lines()
            .filter(|line| line.starts_with("test_duration_seconds_bucket{"))
            .map(|line| {
                line.rsplit_once(' ')
                    .expect("bucket has a value")
                    .1
                    .parse::<u64>()
                    .expect("bucket value is numeric")
            })
            .collect::<Vec<_>>();
        let count = body
            .lines()
            .find_map(|line| {
                line.strip_prefix("test_duration_seconds_count{scope=\"test\"} ")?
                    .parse::<u64>()
                    .ok()
            })
            .expect("histogram count is present");
        assert_eq!(
            buckets.len(),
            super::histogram::DURATION_BUCKETS_MICROS.len() + 1
        );
        assert!(buckets.windows(2).all(|pair| pair[0] <= pair[1]));
        assert_eq!(buckets.last(), Some(&count));
        scrapes = scrapes.saturating_add(1);
        if remaining.load(Ordering::Acquire) == 0 {
            break;
        }
        std::thread::yield_now();
    }
    for worker in workers {
        worker.join().expect("histogram recorder succeeds");
    }
    assert!(scrapes > 0);
    assert_eq!(
        histogram
            .snapshot()
            .buckets
            .into_iter()
            .fold(0u64, u64::saturating_add),
        (WORKERS * RECORDS) as u64
    );
}

#[test]
fn concurrent_recording_loses_no_increments() {
    const WORKERS: usize = 8;
    const RECORDS: usize = 1_000;
    let metrics = Arc::new(Metrics::default());
    let workers = (0..WORKERS)
        .map(|_| {
            let metrics = Arc::clone(&metrics);
            std::thread::spawn(move || {
                for _ in 0..RECORDS {
                    metrics.record_tool(ToolOutcome::Success, Duration::from_millis(1));
                }
            })
        })
        .collect::<Vec<_>>();
    for worker in workers {
        worker.join().expect("metric recorder thread succeeds");
    }
    let body = render(&metrics);
    let expected = WORKERS * RECORDS;
    assert!(body.contains(&format!(
        "ironcrew_tool_calls_total{{outcome=\"success\"}} {expected}\n"
    )));
    assert!(body.contains(&format!(
        "ironcrew_tool_call_duration_seconds_count{{outcome=\"success\"}} {expected}\n"
    )));
}

#[test]
fn bulk_and_durationless_runs_do_not_fabricate_histogram_samples() {
    let metrics = Metrics::default();
    metrics.record_run_count(RunOutcome::Abandoned, 7);
    metrics.record_run(RunOutcome::Abandoned, None);
    let body = render(&metrics);
    assert!(body.contains("ironcrew_runs_total{outcome=\"abandoned\"} 8\n"));
    assert!(body.contains("ironcrew_run_duration_seconds_count{outcome=\"abandoned\"} 0\n"));
}

#[test]
fn run_status_mapping_excludes_in_flight_states() {
    use crate::engine::run_history::RunStatus;

    assert_eq!(
        RunOutcome::from_status(&RunStatus::PartialFailure),
        Some(RunOutcome::PartialFailure)
    );
    assert_eq!(
        RunOutcome::from_status(&RunStatus::Abandoned),
        Some(RunOutcome::Abandoned)
    );
    assert_eq!(RunOutcome::from_status(&RunStatus::Running), None);
    assert_eq!(RunOutcome::from_status(&RunStatus::WaitingForInput), None);
}

#[test]
fn expected_store_outcomes_are_not_failures() {
    use crate::utils::error::IronCrewError;

    assert!(!super::store_error_is_failure(&IronCrewError::Conflict(
        "fenced".into()
    )));
    assert!(!super::store_error_is_failure(
        &IronCrewError::OwnerDraining {
            owner_instance_id: "pod-a".into(),
        }
    ));
    assert!(super::store_error_is_failure(&IronCrewError::Provider(
        "backend unavailable".into()
    )));
}

#[test]
fn terminal_not_applied_is_classified_as_fenced() {
    assert_eq!(
        TerminalOutcome::from_applied(true),
        TerminalOutcome::Success
    );
    assert_eq!(
        TerminalOutcome::from_applied(false),
        TerminalOutcome::Fenced
    );
}
