use super::*;
use ironcrew::usage::UsageTracker;

#[test]
fn finish_then_drop_settles_exactly_once() {
    let tracker = UsageTracker::default();
    tracker.start().unwrap().finish(receipt(10, 5)).unwrap();
    let snapshot = tracker.snapshot().unwrap();
    assert_eq!(snapshot.in_flight, 0);
    assert_eq!(snapshot.settled.requests(), 1);
    assert_eq!(snapshot.settled.total_tokens().known(), Some(15));
}

#[test]
fn active_and_dropped_requests_cannot_appear_complete() {
    let tracker = UsageTracker::default();
    let attempt = tracker.start().unwrap();
    assert_eq!(
        tracker.snapshot().unwrap().coverage,
        UsageCoverage::Unavailable
    );
    drop(attempt);
    tracker.start().unwrap().finish(receipt(10, 5)).unwrap();
    let snapshot = tracker.snapshot().unwrap();
    assert_eq!(snapshot.settled.requests(), 2);
    assert_eq!(snapshot.coverage, UsageCoverage::Partial);
    assert_eq!(snapshot.settled.total_tokens().known(), Some(15));
}

#[tokio::test]
async fn cancelled_future_preserves_observed_usage_before_retry() {
    let tracker = UsageTracker::default();
    let (ready, waiting) = tokio::sync::oneshot::channel();
    let worker_tracker = tracker.clone();
    let worker = tokio::spawn(async move {
        let mut attempt = worker_tracker.start().unwrap();
        attempt.observe(ProviderUsage::OpenAiResponses.parse(
            Some(&json!({
                "input_tokens":10,"output_tokens":2,"total_tokens":12
            })),
            false,
        ));
        ready.send(()).unwrap();
        std::future::pending::<()>().await;
    });
    waiting.await.unwrap();
    worker.abort();
    assert!(worker.await.unwrap_err().is_cancelled());
    tracker.start().unwrap().finish(receipt(20, 3)).unwrap();
    let snapshot = tracker.snapshot().unwrap();
    assert_eq!(snapshot.in_flight, 0);
    assert_eq!(snapshot.settled.requests(), 2);
    assert_eq!(snapshot.settled.total_tokens().known(), Some(35));
    assert_eq!(snapshot.coverage, UsageCoverage::Partial);
}

#[test]
fn output_failure_does_not_discard_a_complete_provider_receipt() {
    let tracker = UsageTracker::default();
    let mut attempt = tracker.start().unwrap();
    attempt.observe(receipt(10, 5));
    drop(attempt); // e.g. blank output rejection after receiving final usage
    assert_eq!(
        tracker.snapshot().unwrap().coverage,
        UsageCoverage::Complete
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn parallel_nested_scopes_share_one_tracker_without_readding_child_results() {
    let tracker = UsageTracker::default();
    let mut workers = tokio::task::JoinSet::new();
    for _ in 0..32 {
        let parent = tracker.clone();
        workers.spawn(async move {
            parent.start().unwrap().finish(receipt(10, 1)).unwrap();
            let nested = parent.clone();
            nested.start().unwrap().finish(receipt(5, 2)).unwrap();
        });
    }
    while let Some(result) = workers.join_next().await {
        result.unwrap();
    }
    let snapshot = tracker.snapshot().unwrap();
    assert_eq!(snapshot.in_flight, 0);
    assert_eq!(snapshot.settled.requests(), 64);
    assert_eq!(snapshot.settled.total_tokens().known(), Some(32 * 18));
    assert_eq!(snapshot.coverage, UsageCoverage::Complete);
}

#[test]
fn overflow_during_cancellation_poison_is_sticky_and_visible() {
    let tracker = UsageTracker::default();
    tracker
        .start()
        .unwrap()
        .finish(receipt(u64::MAX, 0))
        .unwrap();
    let mut attempt = tracker.start().unwrap();
    attempt.observe(receipt(1, 0));
    drop(attempt);
    assert!(tracker.snapshot().is_err());
    assert!(tracker.start().is_err());
}

#[test]
fn separate_runs_do_not_share_accounting() {
    let first = UsageTracker::default();
    let second = UsageTracker::default();
    first.start().unwrap().finish(receipt(10, 5)).unwrap();
    assert_eq!(second.snapshot().unwrap().settled.requests(), 0);
}

#[test]
fn final_error_without_a_receipt_does_not_erase_observed_partial_usage() {
    let tracker = UsageTracker::default();
    let mut attempt = tracker.start().unwrap();
    attempt.observe(ProviderUsage::OpenAiResponses.parse(
        Some(&json!({
            "input_tokens":10,"output_tokens":2,"total_tokens":12
        })),
        false,
    ));
    attempt.finish(UsageReceipt::default()).unwrap();
    let snapshot = tracker.snapshot().unwrap();
    assert_eq!(snapshot.settled.requests(), 1);
    assert_eq!(snapshot.settled.total_tokens().known(), Some(12));
    assert_eq!(snapshot.coverage, UsageCoverage::Partial);
}
