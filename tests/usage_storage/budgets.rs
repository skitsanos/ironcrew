use super::*;
use ironcrew::usage::budget::{BudgetError, BudgetState, TokenBudget};

pub async fn checked_round_trip(store: &dyn StateStore) {
    let tracker = UsageTracker::with_budget(TokenBudget::new(100).unwrap());
    drop(tracker.budget().reserve(20, 30).unwrap());
    tracker.budget().block(BudgetError::Exhausted);
    let usage = tracker.snapshot().unwrap();
    let id = store
        .save_run_intent(RunIntent {
            suggested_id: Some(uuid::Uuid::new_v4().to_string()),
            flow_name: "budget".into(),
            flow: "budget".into(),
            started_at: chrono::Utc::now().to_rfc3339(),
            agent_count: 1,
            task_count: 1,
            tags: vec![],
        })
        .await
        .unwrap();
    let completion = RunCompletion {
        status: RunStatus::Failed,
        finished_at: chrono::Utc::now().to_rfc3339(),
        duration_ms: 1,
        task_results: vec![],
        usage: usage.clone(),
    };
    for forged_budget in [
        ironcrew::usage::budget::BudgetSnapshot {
            charged: 101,
            ..usage.budget.clone()
        },
        ironcrew::usage::budget::BudgetSnapshot {
            state: BudgetState::Disabled,
            ..usage.budget.clone()
        },
    ] {
        let mut forged = completion.clone();
        forged.usage.budget = forged_budget;
        assert!(store.update_run_completion(&id, forged).await.is_err());
    }
    store.update_run_completion(&id, completion).await.unwrap();
    assert_eq!(store.get_run(&id).await.unwrap().usage, usage);
    let summaries = store
        .list_runs_summary(&ListRunsFilter::default(), 100, 0)
        .await
        .unwrap();
    assert_eq!(
        summaries.iter().find(|row| row.run_id == id).unwrap().usage,
        usage
    );
}
