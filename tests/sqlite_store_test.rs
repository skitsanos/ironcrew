//! T1 and T2 for the SQLite backend — intent/completion round-trip
//! and reconcile selectivity. Uses an in-memory SQLite via temp file
//! so no shared state leaks between tests.

use ironcrew::engine::run_history::RunStatus;
use ironcrew::engine::sqlite_store::SqliteStore;
use ironcrew::engine::store::StateStore;
use ironcrew::engine::task::TaskResult;

fn fresh_store() -> SqliteStore {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.db");
    // Leak the tempdir so it outlives the store (store holds no PathBuf
    // reference, but the file needs to exist for the duration of the test).
    let _ = Box::leak(Box::new(dir));
    SqliteStore::new(db_path).unwrap()
}

#[tokio::test]
async fn sqlite_store_intent_completion_roundtrip() {
    let store = fresh_store();

    let run_id = store
        .save_run_intent(
            Some("sqlite-intent-1".into()),
            "demo-flow",
            "2026-04-23T10:00:00Z",
            2,
            3,
            &["dev".into()],
        )
        .await
        .unwrap();
    assert_eq!(run_id, "sqlite-intent-1");

    let r = store.get_run(&run_id).await.unwrap();
    assert_eq!(r.status, RunStatus::Running);
    assert_eq!(r.finished_at, "");
    assert_eq!(r.agent_count, 2);
    assert_eq!(r.task_count, 3);
    assert_eq!(r.tags, vec!["dev".to_string()]);

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
    assert_eq!(r.duration_ms, 5000);
    assert_eq!(r.task_results.len(), 1);
    assert_eq!(r.total_tokens, 100);
}

#[tokio::test]
async fn sqlite_store_reconcile_abandoned_selectivity() {
    use ironcrew::engine::run_history::RunRecord;
    let store = fresh_store();

    // Two Running records via the intent API
    store
        .save_run_intent(Some("r1".into()), "f", "2026-04-23T10:00:00Z", 1, 1, &[])
        .await
        .unwrap();
    store
        .save_run_intent(Some("r2".into()), "f", "2026-04-23T10:01:00Z", 1, 1, &[])
        .await
        .unwrap();

    // One Success, one Failed via legacy save_run (removed in Task 8)
    let success = RunRecord {
        run_id: "s1".into(),
        flow_name: "f".into(),
        status: RunStatus::Success,
        started_at: "2026-04-23T09:00:00Z".into(),
        finished_at: "2026-04-23T09:00:05Z".into(),
        duration_ms: 5000,
        task_results: vec![],
        agent_count: 1,
        task_count: 1,
        total_tokens: 0,
        cached_tokens: 0,
        tags: vec![],
    };
    store.save_run(&success).await.unwrap();

    let failed = RunRecord {
        run_id: "f1".into(),
        status: RunStatus::Failed,
        ..success.clone()
    };
    store.save_run(&failed).await.unwrap();

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

    let second = store
        .reconcile_abandoned_runs("2026-04-23T10:06:00Z")
        .await
        .unwrap();
    assert_eq!(second, 0);
}
