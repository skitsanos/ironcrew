use ironcrew::engine::run_history::{JsonFileStore, ListRunsFilter, RunRecord, RunStatus};
use ironcrew::engine::store::StateStore;
use ironcrew::engine::task::TaskResult;
use ironcrew::utils::error::IronCrewError;

/// Test helper: write a Running intent + immediately update to
/// terminal state. Mirrors the pre-Task 8 `save_run` call shape so
/// list/filter tests don't have to know about the two-phase flow.
async fn save_completed_run(
    store: &JsonFileStore,
    record: &RunRecord,
) -> Result<(), IronCrewError> {
    store
        .save_run_intent(
            Some(record.run_id.clone()),
            &record.flow_name,
            &record.started_at,
            record.agent_count,
            record.task_count,
            &record.tags,
        )
        .await?;
    store
        .update_run_completion(
            &record.run_id,
            record.status.clone(),
            &record.finished_at,
            record.duration_ms,
            record.task_results.clone(),
            record.total_tokens,
            record.cached_tokens,
        )
        .await
}

fn make_record(id: &str, status: RunStatus, started: &str, tags: Vec<String>) -> RunRecord {
    RunRecord {
        run_id: id.into(),
        flow_name: "test".into(),
        status,
        started_at: started.into(),
        finished_at: started.into(),
        duration_ms: 1000,
        task_results: vec![],
        agent_count: 1,
        task_count: 1,
        total_tokens: 0,
        cached_tokens: 0,
        tags,
    }
}

#[tokio::test]
async fn test_save_and_load_run() {
    let dir = tempfile::tempdir().unwrap();
    let history = JsonFileStore::new(dir.path().to_path_buf()).unwrap();

    let record = RunRecord {
        run_id: "test-run-123".into(),
        flow_name: "test flow".into(),
        status: RunStatus::Success,
        started_at: "2026-03-18T12:00:00Z".into(),
        finished_at: "2026-03-18T12:00:05Z".into(),
        duration_ms: 5000,
        task_results: vec![TaskResult {
            task: "task1".into(),
            agent: "agent1".into(),
            output: "done".into(),
            success: true,
            duration_ms: 3000,
            token_usage: None,
            reasoning: None,
        }],
        agent_count: 1,
        task_count: 1,
        total_tokens: 0,
        cached_tokens: 0,
        tags: vec![],
    };

    save_completed_run(&history, &record).await.unwrap();

    let loaded = history.get_run("test-run-123").await.unwrap();
    assert_eq!(loaded.run_id, "test-run-123");
    assert_eq!(loaded.status, RunStatus::Success);
    assert_eq!(loaded.task_results.len(), 1);
}

#[tokio::test]
async fn test_delete_run() {
    let dir = tempfile::tempdir().unwrap();
    let history = JsonFileStore::new(dir.path().to_path_buf()).unwrap();

    let record = RunRecord {
        run_id: "to-delete".into(),
        flow_name: "test".into(),
        status: RunStatus::Success,
        started_at: "2026-03-18T12:00:00Z".into(),
        finished_at: "2026-03-18T12:00:01Z".into(),
        duration_ms: 1000,
        task_results: vec![],
        agent_count: 0,
        task_count: 0,
        total_tokens: 0,
        cached_tokens: 0,
        tags: vec![],
    };
    save_completed_run(&history, &record).await.unwrap();

    history.delete_run("to-delete").await.unwrap();
    assert!(history.get_run("to-delete").await.is_err());
}

#[tokio::test]
async fn test_get_nonexistent_run() {
    let dir = tempfile::tempdir().unwrap();
    let history = JsonFileStore::new(dir.path().to_path_buf()).unwrap();
    assert!(history.get_run("nonexistent").await.is_err());
}

#[tokio::test]
async fn test_list_runs_summary_pagination() {
    let dir = tempfile::tempdir().unwrap();
    let history = JsonFileStore::new(dir.path().to_path_buf()).unwrap();

    for i in 0..5 {
        save_completed_run(
            &history,
            &make_record(
                &format!("run-{}", i),
                RunStatus::Success,
                &format!("2026-03-18T12:00:0{}Z", i),
                vec![],
            ),
        )
        .await
        .unwrap();
    }

    // Page 1 — first 2 newest-first
    let filter = ListRunsFilter::default();
    let page1 = history.list_runs_summary(&filter, 2, 0).await.unwrap();
    assert_eq!(page1.len(), 2);
    assert_eq!(page1[0].run_id, "run-4");
    assert_eq!(page1[1].run_id, "run-3");

    // Page 2 — next 2
    let page2 = history.list_runs_summary(&filter, 2, 2).await.unwrap();
    assert_eq!(page2.len(), 2);
    assert_eq!(page2[0].run_id, "run-2");
    assert_eq!(page2[1].run_id, "run-1");

    // Page 3 — last 1
    let page3 = history.list_runs_summary(&filter, 2, 4).await.unwrap();
    assert_eq!(page3.len(), 1);
    assert_eq!(page3[0].run_id, "run-0");

    // Count — should match total regardless of pagination
    assert_eq!(history.count_runs(&filter).await.unwrap(), 5);
}

#[tokio::test]
async fn test_list_runs_summary_filters() {
    let dir = tempfile::tempdir().unwrap();
    let history = JsonFileStore::new(dir.path().to_path_buf()).unwrap();

    save_completed_run(
        &history,
        &make_record(
            "a",
            RunStatus::Success,
            "2026-03-18T10:00:00Z",
            vec!["prod".into(), "fast".into()],
        ),
    )
    .await
    .unwrap();
    save_completed_run(
        &history,
        &make_record(
            "b",
            RunStatus::Failed,
            "2026-03-18T11:00:00Z",
            vec!["prod".into()],
        ),
    )
    .await
    .unwrap();
    save_completed_run(
        &history,
        &make_record(
            "c",
            RunStatus::Success,
            "2026-03-18T12:00:00Z",
            vec!["dev".into()],
        ),
    )
    .await
    .unwrap();

    // Status filter
    let filter = ListRunsFilter {
        status: Some("success".into()),
        ..Default::default()
    };
    let rows = history.list_runs_summary(&filter, 10, 0).await.unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(history.count_runs(&filter).await.unwrap(), 2);

    // Tag filter
    let filter = ListRunsFilter {
        tag: Some("prod".into()),
        ..Default::default()
    };
    let rows = history.list_runs_summary(&filter, 10, 0).await.unwrap();
    assert_eq!(rows.len(), 2);

    // since filter — only runs at or after 11:00
    let filter = ListRunsFilter {
        since: Some("2026-03-18T11:00:00Z".into()),
        ..Default::default()
    };
    let rows = history.list_runs_summary(&filter, 10, 0).await.unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].run_id, "c");
    assert_eq!(rows[1].run_id, "b");

    // Combined — status=success AND tag=prod → only "a"
    let filter = ListRunsFilter {
        status: Some("success".into()),
        tag: Some("prod".into()),
        ..Default::default()
    };
    let rows = history.list_runs_summary(&filter, 10, 0).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].run_id, "a");
}

#[test]
fn run_status_running_serde_roundtrip() {
    let status = RunStatus::Running;
    let json = serde_json::to_string(&status).unwrap();
    assert_eq!(json, "\"Running\"");
    let back: RunStatus = serde_json::from_str(&json).unwrap();
    assert_eq!(back, RunStatus::Running);
    assert_eq!(format!("{}", status), "running");
}

#[test]
fn run_status_abandoned_serde_roundtrip() {
    let status = RunStatus::Abandoned;
    let json = serde_json::to_string(&status).unwrap();
    assert_eq!(json, "\"Abandoned\"");
    let back: RunStatus = serde_json::from_str(&json).unwrap();
    assert_eq!(back, RunStatus::Abandoned);
    assert_eq!(format!("{}", status), "abandoned");
}

#[test]
fn run_status_existing_variants_unchanged() {
    // Regression guard: the three existing variants must still deserialize
    // from their existing JSON representation (no breaking change to
    // already-persisted records).
    let success: RunStatus = serde_json::from_str("\"Success\"").unwrap();
    assert_eq!(success, RunStatus::Success);
    let partial: RunStatus = serde_json::from_str("\"PartialFailure\"").unwrap();
    assert_eq!(partial, RunStatus::PartialFailure);
    let failed: RunStatus = serde_json::from_str("\"Failed\"").unwrap();
    assert_eq!(failed, RunStatus::Failed);
}

#[tokio::test]
async fn json_store_intent_completion_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let store = JsonFileStore::new(dir.path().to_path_buf()).unwrap();

    let run_id = store
        .save_run_intent(
            Some("test-intent-1".into()),
            "demo-flow",
            "2026-04-23T10:00:00Z",
            2,
            3,
            &["dev".into()],
        )
        .await
        .unwrap();
    assert_eq!(run_id, "test-intent-1");

    let r = store.get_run(&run_id).await.unwrap();
    assert_eq!(r.status, RunStatus::Running);
    assert_eq!(r.finished_at, "");
    assert_eq!(r.duration_ms, 0);
    assert_eq!(r.agent_count, 2);
    assert_eq!(r.task_count, 3);
    assert!(r.task_results.is_empty());

    store
        .update_run_completion(
            &run_id,
            RunStatus::Success,
            "2026-04-23T10:00:05Z",
            5000,
            vec![TaskResult {
                task: "answer".into(),
                agent: "assistant".into(),
                output: "hi".into(),
                success: true,
                duration_ms: 4500,
                token_usage: None,
                reasoning: None,
            }],
            100,
            20,
        )
        .await
        .unwrap();

    let r = store.get_run(&run_id).await.unwrap();
    assert_eq!(r.status, RunStatus::Success);
    assert_eq!(r.finished_at, "2026-04-23T10:00:05Z");
    assert_eq!(r.duration_ms, 5000);
    assert_eq!(r.task_results.len(), 1);
    assert_eq!(r.total_tokens, 100);
    assert_eq!(r.cached_tokens, 20);
}

#[tokio::test]
async fn json_store_reconcile_abandoned_selectivity() {
    let dir = tempfile::tempdir().unwrap();
    let store = JsonFileStore::new(dir.path().to_path_buf()).unwrap();

    // Two Running records
    store
        .save_run_intent(Some("r1".into()), "f", "2026-04-23T10:00:00Z", 1, 1, &[])
        .await
        .unwrap();
    store
        .save_run_intent(Some("r2".into()), "f", "2026-04-23T10:01:00Z", 1, 1, &[])
        .await
        .unwrap();

    // One Success, one Failed — use the two-phase helper so save_run
    // (removed in Task 8) is no longer needed.
    save_completed_run(
        &store,
        &make_record("s1", RunStatus::Success, "2026-04-23T09:00:00Z", vec![]),
    )
    .await
    .unwrap();
    save_completed_run(
        &store,
        &make_record("f1", RunStatus::Failed, "2026-04-23T09:30:00Z", vec![]),
    )
    .await
    .unwrap();

    let count = store
        .reconcile_abandoned_runs("2026-04-23T10:05:00Z")
        .await
        .unwrap();
    assert_eq!(count, 2);

    assert_eq!(
        store.get_run("r1").await.unwrap().status,
        RunStatus::Abandoned
    );
    assert_eq!(
        store.get_run("r1").await.unwrap().finished_at,
        "2026-04-23T10:05:00Z"
    );
    assert_eq!(
        store.get_run("r2").await.unwrap().status,
        RunStatus::Abandoned
    );
    assert_eq!(
        store.get_run("s1").await.unwrap().status,
        RunStatus::Success
    );
    assert_eq!(store.get_run("f1").await.unwrap().status, RunStatus::Failed);

    // Idempotency: a second call finds nothing.
    let second = store
        .reconcile_abandoned_runs("2026-04-23T10:06:00Z")
        .await
        .unwrap();
    assert_eq!(second, 0);
}
