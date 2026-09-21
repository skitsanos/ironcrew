use ironcrew::usage::budget::{BudgetError, BudgetState, MAX_RUN_TOKENS, TokenBudget};
use ironcrew::usage::{UsageCounts, UsageReceipt, UsageSnapshot, UsageTracker};

fn receipt(input: u64, output: u64, final_receipt: bool) -> UsageReceipt {
    UsageReceipt::from_counts(
        UsageCounts {
            prompt_tokens: Some(input),
            completion_tokens: Some(output),
            total_tokens: Some(input + output),
            ..Default::default()
        },
        final_receipt,
    )
}

struct MissingReceiptOwnership;
#[async_trait::async_trait]
impl ironcrew::llm::provider::LlmProvider for MissingReceiptOwnership {
    fn supports_token_budget(&self) -> bool {
        true
    }
    async fn chat(
        &self,
        _: ironcrew::llm::provider::ChatRequest,
    ) -> ironcrew::utils::error::Result<ironcrew::llm::provider::ChatResponse> {
        panic!("an incomplete custom-provider contract must never dispatch")
    }
    async fn chat_with_tools(
        &self,
        request: ironcrew::llm::provider::ChatRequest,
        _: &[ironcrew::llm::provider::ToolSchema],
    ) -> ironcrew::utils::error::Result<ironcrew::llm::provider::ChatResponse> {
        self.chat(request).await
    }
}

#[tokio::test]
async fn custom_budget_capability_also_requires_checked_receipt_ownership() {
    let scope = UsageTracker::with_budget(TokenBudget::new(100).unwrap());
    let provider = ironcrew::llm::scope::with_usage_tracker(
        std::sync::Arc::new(MissingReceiptOwnership),
        scope.clone(),
    );
    let request = ironcrew::engine::agent::Agent::default().chat_request("fixture".into(), vec![]);
    assert!(provider.chat(request).await.is_err());
    assert_eq!(scope.budget().snapshot().state, BudgetState::Unsupported);
    assert_eq!(scope.snapshot().unwrap().settled.requests(), 0);
}

#[test]
fn opt_in_configuration_is_strict_and_bounded() {
    assert!(!TokenBudget::from_raw(None).unwrap().enabled());
    for invalid in [
        "",
        "0",
        "-1",
        "+1",
        " 1",
        "1.0",
        "none",
        "1000000001",
        "18446744073709551616",
    ] {
        assert_eq!(
            TokenBudget::from_raw(Some(invalid)).unwrap_err(),
            BudgetError::Configuration
        );
    }
    for valid in [1, MAX_RUN_TOKENS] {
        assert!(TokenBudget::new(valid).is_ok());
    }
}

#[test]
fn exact_boundary_and_complete_receipt_reconciliation() {
    let budget = TokenBudget::new(100).unwrap();
    let first = budget.reserve(20, 80).unwrap();
    assert_eq!(budget.snapshot().reserved, 100);
    first.finish(&receipt(20, 5, true)).unwrap();
    assert_eq!(budget.snapshot().charged, 25);
    budget
        .reserve(70, 5)
        .unwrap()
        .finish(&receipt(70, 5, true))
        .unwrap();
    budget.check().unwrap(); // exact spend may complete successfully
    assert_eq!(budget.reserve(0, 1).unwrap_err(), BudgetError::Exhausted);
    assert_eq!(budget.snapshot().state, BudgetState::Exhausted);
}

#[test]
fn unknown_partial_and_cancelled_attempts_retain_full_reservation() {
    let budget = TokenBudget::new(100).unwrap();
    drop(budget.reserve(10, 10).unwrap());
    budget
        .reserve(10, 10)
        .unwrap()
        .finish(&UsageReceipt::default())
        .unwrap();
    budget
        .reserve(10, 10)
        .unwrap()
        .finish(&receipt(2, 1, false))
        .unwrap();
    let snap = budget.snapshot();
    assert_eq!(
        (snap.charged, snap.retained, snap.reserved, snap.in_flight),
        (60, 60, 0, 0)
    );
    snap.validate().unwrap();
}

#[test]
fn denial_is_sticky_when_other_work_finishes_and_frees_tokens() {
    let budget = TokenBudget::new(20).unwrap();
    let first = budget.reserve(10, 10).unwrap();
    assert!(budget.reserve(1, 1).is_err());
    first.finish(&receipt(0, 0, true)).unwrap();
    assert_eq!(budget.snapshot().charged, 0);
    assert_eq!(budget.reserve(1, 1).unwrap_err(), BudgetError::Exhausted);
}

#[test]
fn invalid_bounds_and_provider_overruns_block_future_calls() {
    for (input, output) in [(11, 0), (0, 11), (9, 9)] {
        let budget = TokenBudget::new(100).unwrap();
        assert_eq!(
            budget
                .reserve(10, 5)
                .unwrap()
                .finish(&receipt(input, output, true))
                .unwrap_err(),
            BudgetError::BoundViolated
        );
        assert_eq!(budget.snapshot().charged, 15);
        assert_eq!(budget.check(), Err(BudgetError::BoundViolated));
    }
    let budget = TokenBudget::new(100).unwrap();
    assert!(budget.reserve(u64::MAX, 1).is_err());
    assert!(TokenBudget::new(100).unwrap().reserve(0, 0).is_err());
}

#[test]
fn concurrent_admission_never_overcommits() {
    let budget = TokenBudget::new(100).unwrap();
    let admitted = std::sync::atomic::AtomicU64::new(0);
    std::thread::scope(|scope| {
        for _ in 0..32 {
            let budget = &budget;
            let admitted = &admitted;
            scope.spawn(move || {
                if let Ok(reservation) = budget.reserve(4, 6) {
                    admitted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    drop(reservation);
                }
            });
        }
    });
    assert_eq!(admitted.load(std::sync::atomic::Ordering::SeqCst), 10);
    assert_eq!(budget.snapshot().charged, 100);
    assert_eq!(budget.snapshot().state, BudgetState::Exhausted);
}

#[test]
fn children_share_budget_but_session_observers_and_new_runs_do_not() {
    let root = UsageTracker::with_budget(TokenBudget::new(100).unwrap());
    let session = UsageTracker::default();
    let child = root.child().unwrap().child_observed_by(&session).unwrap();
    drop(child.budget().reserve(10, 10).unwrap());
    assert_eq!(root.snapshot().unwrap().budget.charged, 20);
    assert_eq!(
        session.snapshot().unwrap().budget.state,
        BudgetState::Disabled
    );
    let independent = UsageTracker::with_budget(TokenBudget::new(100).unwrap());
    assert_eq!(independent.snapshot().unwrap().budget.charged, 0);
    let restored = UsageTracker::from_snapshot(session.snapshot().unwrap()).unwrap();
    assert!(!restored.budget().enabled());
}

#[test]
fn budget_snapshot_is_explicit_checked_and_lossless() {
    let tracker = UsageTracker::with_budget(TokenBudget::new(12345).unwrap());
    drop(tracker.budget().reserve(100, 20).unwrap());
    let snapshot = tracker.snapshot().unwrap();
    let wire = serde_json::to_value(&snapshot).unwrap();
    assert_eq!(wire["budget"]["charged"], "120");
    assert_eq!(
        serde_json::from_value::<UsageSnapshot>(wire.clone()).unwrap(),
        snapshot
    );
    for (field, value) in [
        ("charged", serde_json::json!("999999")),
        ("state", serde_json::json!("disabled")),
        ("in_flight", serde_json::json!("1")),
    ] {
        let mut bad = wire.clone();
        bad["budget"][field] = value;
        assert!(serde_json::from_value::<UsageSnapshot>(bad).is_err());
    }
}
