use ironcrew::engine::run_history::{
    JsonFileStore, ListRunsFilter, RunCompletion, RunIntent, RunStatus, RunTransition,
};
use ironcrew::engine::sqlite_store::SqliteStore;
use ironcrew::engine::store::StateStore;
use ironcrew::engine::task::TaskResult;
use ironcrew::usage::{UsageCounts, UsageCoverage, UsageReceipt, UsageSnapshot, UsageTracker};

async fn checked_round_trip(store: &dyn StateStore) {
    let tracker = UsageTracker::default();
    tracker
        .start()
        .unwrap()
        .finish(UsageReceipt::from_counts(
            UsageCounts {
                prompt_tokens: Some(u64::MAX - 10),
                completion_tokens: Some(10),
                total_tokens: Some(u64::MAX),
                cached_tokens: Some(100),
                reasoning_tokens: Some(7),
                ..Default::default()
            },
            true,
        ))
        .unwrap();
    tracker
        .start()
        .unwrap()
        .finish(UsageReceipt::default())
        .unwrap();
    let usage = tracker.snapshot().unwrap();
    assert_eq!(usage.coverage, UsageCoverage::Partial);
    let run_id = store
        .save_run_intent(RunIntent {
            suggested_id: Some(uuid::Uuid::new_v4().to_string()),
            flow_name: "usage".into(),
            flow: "usage".into(),
            started_at: chrono::Utc::now().to_rfc3339(),
            agent_count: 1,
            task_count: 1,
            tags: vec!["checked".into()],
        })
        .await
        .unwrap();
    assert_eq!(
        store.get_run(&run_id).await.unwrap().usage,
        UsageSnapshot::unavailable()
    );
    let completion = RunCompletion {
        status: RunStatus::Failed,
        finished_at: chrono::Utc::now().to_rfc3339(),
        duration_ms: 1,
        task_results: vec![TaskResult {
            task: "failed".into(),
            agent: "one".into(),
            output: "provider failed after a receipt".into(),
            success: false,
            duration_ms: 1,
            usage: usage.clone(),
            reasoning: None,
        }],
        usage: usage.clone(),
    };
    let mut forged = completion.clone();
    forged.usage.coverage = UsageCoverage::Complete;
    assert!(store.update_run_completion(&run_id, forged).await.is_err());
    let mut forged = completion.clone();
    forged.task_results[0].usage.coverage = UsageCoverage::Complete;
    assert!(store.update_run_completion(&run_id, forged).await.is_err());
    assert_eq!(
        store.get_run(&run_id).await.unwrap().status,
        RunStatus::Running
    );
    assert_eq!(
        store
            .update_run_completion(&run_id, completion)
            .await
            .unwrap(),
        RunTransition::Applied
    );
    let loaded = store.get_run(&run_id).await.unwrap();
    assert_eq!(loaded.usage, usage);
    assert_eq!(loaded.task_results[0].usage, usage);
    assert_eq!(loaded.tags, ["checked"]);
    let summary = store
        .list_runs_summary(&ListRunsFilter::default(), 100, 0)
        .await
        .unwrap();
    assert_eq!(
        summary
            .iter()
            .find(|item| item.run_id == run_id)
            .unwrap()
            .usage,
        usage
    );
    let wire = serde_json::to_value(&loaded).unwrap();
    assert_eq!(
        wire["usage"]["settled"]["total_tokens"]["known"],
        u64::MAX.to_string()
    );
    assert_eq!(
        wire["usage"]["settled"]["cache_write_tokens"]["known"],
        serde_json::Value::Null
    );
    assert!(wire.get("total_tokens").is_none());
    assert!(wire["task_results"][0].get("token_usage").is_none());
    let superseded = RunCompletion {
        status: RunStatus::Success,
        finished_at: chrono::Utc::now().to_rfc3339(),
        duration_ms: 2,
        task_results: vec![],
        usage: UsageSnapshot::default(),
    };
    assert_eq!(
        store
            .update_run_completion(&run_id, superseded)
            .await
            .unwrap(),
        RunTransition::AlreadyTerminal(RunStatus::Failed)
    );
    assert_eq!(store.get_run(&run_id).await.unwrap().usage, usage);
}

#[tokio::test]
async fn json_preserves_checked_usage_and_rejects_forged_coverage() {
    let root = tempfile::tempdir().unwrap();
    let store = JsonFileStore::new(root.path().to_owned()).unwrap();
    checked_round_trip(&store).await;
}

#[tokio::test]
async fn sqlite_preserves_checked_usage_and_rejects_forged_coverage() {
    let root = tempfile::tempdir().unwrap();
    let store = SqliteStore::new(root.path().join("usage.db")).unwrap();
    checked_round_trip(&store).await;
    let conn = rusqlite::Connection::open(root.path().join("usage.db")).unwrap();
    conn.execute("UPDATE runs SET usage = ?1", ["x".repeat(4097)])
        .unwrap();
    let error = store
        .list_runs_summary(&ListRunsFilter::default(), 100, 0)
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("Null"),
        "oversize payload must become SQL NULL, not reach the JSON decoder: {error}"
    );
}

#[cfg(feature = "postgres")]
#[tokio::test]
async fn postgres_preserves_checked_usage_and_rejects_forged_coverage() {
    let Ok(url) = std::env::var("IRONCREW_TEST_PG_URL") else {
        eprintln!("SKIP live usage persistence: IRONCREW_TEST_PG_URL unset");
        return;
    };
    let store = ironcrew::engine::postgres_store::PostgresStore::new(&url, "ic046_usage_")
        .await
        .unwrap();
    checked_round_trip(&store).await;
    let pool = sqlx::PgPool::connect(&url).await.unwrap();
    sqlx::query("UPDATE ic046_usage_runs SET usage = $1")
        .bind(sqlx::types::Json(
            serde_json::json!({"oversized": "x".repeat(4097)}),
        ))
        .execute(&pool)
        .await
        .unwrap();
    let error = store
        .list_runs_summary(&ListRunsFilter::default(), 100, 0)
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("unexpected null"),
        "oversize payload must become SQL NULL: {error}"
    );
    // These rows belong only to this test; remove its deliberately corrupt fixtures.
    sqlx::query("DELETE FROM ic046_usage_runs")
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;
}
