use ironcrew::engine::run_history::{ListRunsFilter, RunHistory, RunRecord, RunStatus};
use ironcrew::engine::store::StateStore;
use ironcrew::engine::task::TaskResult;

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
            reasoning: None,
        }],
        agent_count: 1,
        task_count: 1,
        total_tokens: 0,
        cached_tokens: 0,
        tags: vec![],
    };

    history.save_run(&record).await.unwrap();

    let loaded = history.get_run("test-run-123").await.unwrap();
    assert_eq!(loaded.run_id, "test-run-123");
    assert_eq!(loaded.status, RunStatus::Success);
    assert_eq!(loaded.task_results.len(), 1);
}

#[tokio::test]
async fn test_list_runs() {
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
            tags: vec![],
        };
        history.save_run(&record).await.unwrap();
    }

    let all = history.list_runs(None).await.unwrap();
    assert_eq!(all.len(), 3);

    let success_only = history.list_runs(Some("success")).await.unwrap();
    assert_eq!(success_only.len(), 2);

    let failed_only = history.list_runs(Some("failed")).await.unwrap();
    assert_eq!(failed_only.len(), 1);
}

#[tokio::test]
async fn test_delete_run() {
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
        tags: vec![],
    };
    history.save_run(&record).await.unwrap();

    history.delete_run("to-delete").await.unwrap();
    assert!(history.get_run("to-delete").await.is_err());
}

#[tokio::test]
async fn test_get_nonexistent_run() {
    let dir = tempfile::tempdir().unwrap();
    let history = RunHistory::new(dir.path().to_path_buf()).unwrap();
    assert!(history.get_run("nonexistent").await.is_err());
}

#[tokio::test]
async fn test_list_runs_summary_pagination() {
    let dir = tempfile::tempdir().unwrap();
    let history = RunHistory::new(dir.path().to_path_buf()).unwrap();

    for i in 0..5 {
        history
            .save_run(&make_record(
                &format!("run-{}", i),
                RunStatus::Success,
                &format!("2026-03-18T12:00:0{}Z", i),
                vec![],
            ))
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
    let history = RunHistory::new(dir.path().to_path_buf()).unwrap();

    history
        .save_run(&make_record(
            "a",
            RunStatus::Success,
            "2026-03-18T10:00:00Z",
            vec!["prod".into(), "fast".into()],
        ))
        .await
        .unwrap();
    history
        .save_run(&make_record(
            "b",
            RunStatus::Failed,
            "2026-03-18T11:00:00Z",
            vec!["prod".into()],
        ))
        .await
        .unwrap();
    history
        .save_run(&make_record(
            "c",
            RunStatus::Success,
            "2026-03-18T12:00:00Z",
            vec!["dev".into()],
        ))
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
