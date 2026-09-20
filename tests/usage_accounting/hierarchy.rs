use super::*;
use ironcrew::usage::UsageTracker;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn disjoint_children_update_inclusive_ancestors_once() {
    let root = UsageTracker::default();
    let mut workers = tokio::task::JoinSet::new();
    for _ in 0..32 {
        let child = root.child().unwrap();
        workers.spawn(async move {
            child.start().unwrap().finish(receipt(10, 1)).unwrap();
            let grandchild = child.child().unwrap();
            let alias = grandchild.clone();
            alias.start().unwrap().finish(receipt(5, 2)).unwrap();
            assert_eq!(grandchild.snapshot().unwrap().settled.requests(), 1);
            assert_eq!(child.snapshot().unwrap().settled.requests(), 2);
        });
    }
    while let Some(result) = workers.join_next().await {
        result.unwrap();
    }
    let snapshot = root.snapshot().unwrap();
    assert_eq!(snapshot.in_flight, 0);
    assert_eq!(snapshot.settled.requests(), 64);
    assert_eq!(snapshot.settled.total_tokens().known(), Some(32 * 18));
    assert_eq!(snapshot.coverage, UsageCoverage::Complete);
}

#[test]
fn cancellation_and_in_flight_state_reach_every_ancestor() {
    let root = UsageTracker::default();
    let child = root.child().unwrap();
    let sibling = root.child().unwrap();
    let mut attempt = child.start().unwrap();
    for scope in [&root, &child] {
        assert_eq!(scope.snapshot().unwrap().in_flight, 1);
        assert_eq!(
            scope.snapshot().unwrap().coverage,
            UsageCoverage::Unavailable
        );
    }
    attempt.observe(ProviderUsage::OpenAiResponses.parse(
        Some(&json!({"input_tokens":10,"output_tokens":2,"total_tokens":12})),
        false,
    ));
    drop(attempt);
    for scope in [&root, &child] {
        let snapshot = scope.snapshot().unwrap();
        assert_eq!(snapshot.in_flight, 0);
        assert_eq!(snapshot.settled.requests(), 1);
        assert_eq!(snapshot.settled.total_tokens().known(), Some(12));
        assert_eq!(snapshot.coverage, UsageCoverage::Partial);
    }
    assert_eq!(sibling.snapshot().unwrap().settled.requests(), 0);
}

#[test]
fn parent_overflow_poisons_active_lineage_and_blocks_sibling_dispatch() {
    let root = UsageTracker::default();
    let child = root.child().unwrap();
    let sibling = root.child().unwrap();
    let pending = sibling.start().unwrap();
    root.start().unwrap().finish(receipt(u64::MAX, 0)).unwrap();
    assert!(child.start().unwrap().finish(receipt(1, 0)).is_err());
    assert!(root.snapshot().is_err());
    assert!(child.snapshot().is_err());
    // Its own observed state remains readable, but the failed parent forbids new work.
    assert_eq!(sibling.snapshot().unwrap().in_flight, 1);
    assert!(sibling.start().is_err());
    assert_eq!(sibling.snapshot().unwrap().in_flight, 1);
    drop(pending);
    assert!(sibling.snapshot().is_err());
}

#[test]
fn scope_depth_is_bounded_without_mutating_the_parent() {
    let root = UsageTracker::default();
    let mut leaf = root.clone();
    for _ in 0..64 {
        leaf = leaf.child().unwrap();
    }
    assert!(leaf.child().is_err());
    leaf.start().unwrap().finish(receipt(1, 0)).unwrap();
    assert_eq!(root.snapshot().unwrap().settled.requests(), 1);
}
