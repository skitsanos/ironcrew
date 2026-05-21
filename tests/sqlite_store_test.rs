//! T1 and T2 for the SQLite backend — intent/completion round-trip
//! and reconcile selectivity. Uses an in-memory SQLite via temp file
//! so no shared state leaks between tests.

use ironcrew::engine::run_history::{RunRecord, RunStatus};
use ironcrew::engine::sqlite_store::SqliteStore;
use ironcrew::engine::store::StateStore;
use ironcrew::engine::task::TaskResult;
use ironcrew::utils::error::IronCrewError;

/// Test helper: write a Running intent + immediately update to
/// terminal state. Mirrors the pre-Task 8 `save_run` call shape so
/// reconcile-selectivity tests don't have to know about the two-phase flow.
async fn save_completed_run(store: &SqliteStore, record: &RunRecord) -> Result<(), IronCrewError> {
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

    // One Success, one Failed — use the two-phase helper so save_run
    // (removed in Task 8) is no longer needed.
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
    save_completed_run(&store, &success).await.unwrap();

    let failed = RunRecord {
        run_id: "f1".into(),
        status: RunStatus::Failed,
        ..success.clone()
    };
    save_completed_run(&store, &failed).await.unwrap();

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

#[tokio::test]
async fn sqlite_store_audit_event_roundtrip() {
    use ironcrew::engine::audit::{AuditEvent, AuditFilter};

    let store = fresh_store();

    let event = AuditEvent {
        id: String::new(),
        timestamp: "2026-05-21T10:00:00Z".into(),
        action: "flow.run.delete".into(),
        flow_path: Some("chat-http".into()),
        target: Some("run-xyz".into()),
        actor: Some("alice@example.com".into()),
        source_ip: Some("203.0.113.7".into()),
        success: true,
        status_code: 200,
        metadata: Some(serde_json::json!({"tags": ["prod"]})),
    };

    let id = store.save_audit_event(&event).await.unwrap();
    assert!(!id.is_empty());

    let events = store
        .list_audit_events(&AuditFilter::default(), 10, 0)
        .await
        .unwrap();
    assert_eq!(events.len(), 1);
    let saved = &events[0];
    assert_eq!(saved.id, id);
    assert_eq!(saved.action, "flow.run.delete");
    assert_eq!(saved.flow_path.as_deref(), Some("chat-http"));
    assert!(saved.success);
    assert_eq!(saved.status_code, 200);
    assert_eq!(saved.metadata, Some(serde_json::json!({"tags": ["prod"]})));
}

#[tokio::test]
async fn sqlite_store_audit_filter_selectivity() {
    use ironcrew::engine::audit::{AuditEvent, AuditFilter};

    let store = fresh_store();

    for (i, (flow, action, actor, success)) in [
        ("flow-a", "flow.run.start", "alice", true),
        ("flow-a", "flow.run.delete", "alice", true),
        ("flow-b", "flow.run.start", "bob", false),
        ("flow-b", "conversation.start", "alice", true),
        ("flow-a", "conversation.delete", "bob", false),
    ]
    .iter()
    .enumerate()
    {
        store
            .save_audit_event(&AuditEvent {
                id: String::new(),
                timestamp: format!("2026-05-21T10:0{}:00Z", i),
                action: (*action).into(),
                flow_path: Some((*flow).into()),
                target: None,
                actor: Some((*actor).into()),
                source_ip: None,
                success: *success,
                status_code: if *success { 200 } else { 500 },
                metadata: None,
            })
            .await
            .unwrap();
    }

    assert_eq!(
        store
            .list_audit_events(&AuditFilter::default(), 0, 0)
            .await
            .unwrap()
            .len(),
        5
    );
    assert_eq!(
        store
            .list_audit_events(
                &AuditFilter {
                    flow_path: Some("flow-a".into()),
                    ..Default::default()
                },
                0,
                0
            )
            .await
            .unwrap()
            .len(),
        3
    );
    assert_eq!(
        store
            .list_audit_events(
                &AuditFilter {
                    action: Some("flow.run.start".into()),
                    ..Default::default()
                },
                0,
                0
            )
            .await
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        store
            .list_audit_events(
                &AuditFilter {
                    success: Some(false),
                    ..Default::default()
                },
                0,
                0
            )
            .await
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        store
            .count_audit_events(&AuditFilter {
                flow_path: Some("flow-a".into()),
                ..Default::default()
            })
            .await
            .unwrap(),
        3
    );
}

#[tokio::test]
async fn sqlite_store_audit_pagination_newest_first() {
    use ironcrew::engine::audit::{AuditEvent, AuditFilter};

    let store = fresh_store();

    for i in 0..12 {
        store
            .save_audit_event(&AuditEvent {
                id: String::new(),
                timestamp: format!("2026-05-21T10:{:02}:00Z", i),
                action: format!("action-{i}"),
                flow_path: None,
                target: None,
                actor: None,
                source_ip: None,
                success: true,
                status_code: 200,
                metadata: None,
            })
            .await
            .unwrap();
    }

    let page1 = store
        .list_audit_events(&AuditFilter::default(), 5, 0)
        .await
        .unwrap();
    assert_eq!(page1.len(), 5);
    assert_eq!(page1[0].action, "action-11");
    assert_eq!(page1[4].action, "action-7");

    let page2 = store
        .list_audit_events(&AuditFilter::default(), 5, 5)
        .await
        .unwrap();
    assert_eq!(page2.len(), 5);
    assert_eq!(page2[0].action, "action-6");

    assert_eq!(
        store
            .count_audit_events(&AuditFilter::default())
            .await
            .unwrap(),
        12
    );
}
