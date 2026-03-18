use ironcrew::engine::run_history::{RunHistory, RunRecord, RunStatus};
use ironcrew::engine::task::TaskResult;

#[test]
fn test_save_and_load_run() {
    let dir = tempfile::tempdir().unwrap();
    let history = RunHistory::new(dir.path().to_path_buf()).unwrap();

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
        }],
        agent_count: 1,
        task_count: 1,
        total_tokens: 0,
        cached_tokens: 0,
    };

    history.save(&record).unwrap();

    let loaded = history.get("test-run-123").unwrap();
    assert_eq!(loaded.run_id, "test-run-123");
    assert_eq!(loaded.status, RunStatus::Success);
    assert_eq!(loaded.task_results.len(), 1);
}

#[test]
fn test_list_runs() {
    let dir = tempfile::tempdir().unwrap();
    let history = RunHistory::new(dir.path().to_path_buf()).unwrap();

    for i in 0..3 {
        let record = RunRecord {
            run_id: format!("run-{}", i),
            flow_name: "test".into(),
            status: if i == 1 {
                RunStatus::Failed
            } else {
                RunStatus::Success
            },
            started_at: format!("2026-03-18T12:00:0{}Z", i),
            finished_at: format!("2026-03-18T12:00:0{}Z", i + 1),
            duration_ms: 1000,
            task_results: vec![],
            agent_count: 1,
            task_count: 1,
            total_tokens: 0,
            cached_tokens: 0,
        };
        history.save(&record).unwrap();
    }

    let all = history.list(None).unwrap();
    assert_eq!(all.len(), 3);

    let success_only = history.list(Some("success")).unwrap();
    assert_eq!(success_only.len(), 2);

    let failed_only = history.list(Some("failed")).unwrap();
    assert_eq!(failed_only.len(), 1);
}

#[test]
fn test_delete_run() {
    let dir = tempfile::tempdir().unwrap();
    let history = RunHistory::new(dir.path().to_path_buf()).unwrap();

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
    };
    history.save(&record).unwrap();

    history.delete("to-delete").unwrap();
    assert!(history.get("to-delete").is_err());
}

#[test]
fn test_get_nonexistent_run() {
    let dir = tempfile::tempdir().unwrap();
    let history = RunHistory::new(dir.path().to_path_buf()).unwrap();
    assert!(history.get("nonexistent").is_err());
}
