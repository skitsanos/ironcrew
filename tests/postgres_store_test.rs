//! Live-database integration tests for the Postgres backend.
//!
//! Skipped unless `IRONCREW_TEST_PG_URL` points at a reachable PostgreSQL
//! instance, e.g.:
//!
//!   docker run -d --name pg -e POSTGRES_PASSWORD=ironcrew -e POSTGRES_USER=ironcrew \
//!     -e POSTGRES_DB=ironcrew_test -p 55432:5432 postgres:17
//!   IRONCREW_TEST_PG_URL=postgres://ironcrew:ironcrew@localhost:55432/ironcrew_test \
//!     cargo test --all-features --test postgres_store_test
//!
//! Each test uses its own table prefix and drops those tables first, so they
//! are isolated and can run in parallel against one database.
#![cfg(feature = "postgres")]

use ironcrew::engine::audit::{AuditEvent, AuditFilter};
use ironcrew::engine::postgres_store::PostgresStore;
use ironcrew::engine::run_history::{
    ListRunsFilter, RunCompletion, RunIntent, RunStatus,
};
use ironcrew::engine::store::StateStore;
use ironcrew::engine::task::TaskResult;

fn pg_url() -> Option<String> {
    std::env::var("IRONCREW_TEST_PG_URL").ok().filter(|s| !s.is_empty())
}

/// Drop this test's prefixed tables so it starts from a clean schema even when
/// the database is reused across runs.
async fn reset(url: &str, prefix: &str) {
    let pool = sqlx::PgPool::connect(url).await.expect("connect for reset");
    for t in ["runs", "conversations", "dialogs", "audit_events"] {
        let sql = format!("DROP TABLE IF EXISTS {prefix}{t} CASCADE");
        sqlx::query(sqlx::AssertSqlSafe(sql)).execute(&pool).await.expect("drop table");
    }
    pool.close().await;
}

fn intent(id: &str, flow: &str, started_at: &str, tags: Vec<String>) -> RunIntent {
    RunIntent {
        suggested_id: Some(id.into()),
        flow_name: "goal".into(),
        flow: flow.into(),
        started_at: started_at.into(),
        agent_count: 1,
        task_count: 1,
        tags,
    }
}

#[tokio::test]
async fn pg_intent_completion_roundtrip() {
    let Some(url) = pg_url() else {
        eprintln!("SKIP pg_intent_completion_roundtrip: IRONCREW_TEST_PG_URL unset");
        return;
    };
    let prefix = "rt_";
    reset(&url, prefix).await;
    let store = PostgresStore::new(&url, prefix).await.unwrap();

    let run_id = store
        .save_run_intent(intent("pg-1", "demo", "2026-04-23T10:00:00Z", vec!["dev".into()]))
        .await
        .unwrap();
    assert_eq!(run_id, "pg-1");

    let r = store.get_run(&run_id).await.unwrap();
    assert_eq!(r.status, RunStatus::Running);
    assert_eq!(r.flow, "demo");
    assert_eq!(r.tags, vec!["dev".to_string()]);
    assert_eq!(r.finished_at, "");

    store
        .update_run_completion(
            &run_id,
            RunCompletion {
                status: RunStatus::Success,
                finished_at: "2026-04-23T10:00:05Z".into(),
                duration_ms: 5000,
                task_results: vec![TaskResult {
                    task: "answer".into(),
                    agent: "assistant".into(),
                    output: "hi".into(),
                    success: true,
                    duration_ms: 4500,
                    token_usage: None,
                    reasoning: None,
                }],
                total_tokens: 100,
                cached_tokens: 20,
            },
        )
        .await
        .unwrap();

    let r = store.get_run(&run_id).await.unwrap();
    assert_eq!(r.status, RunStatus::Success);
    assert_eq!(r.duration_ms, 5000);
    assert_eq!(r.task_results.len(), 1);
    assert_eq!(r.total_tokens, 100);

    // Double completion is rejected (record is no longer Running).
    let again = store
        .update_run_completion(
            &run_id,
            RunCompletion {
                status: RunStatus::Failed,
                finished_at: "2026-04-23T10:00:06Z".into(),
                duration_ms: 6000,
                task_results: vec![],
                total_tokens: 0,
                cached_tokens: 0,
            },
        )
        .await;
    assert!(again.is_err());
}

/// H2: runs scoped by flow slug on a real database.
#[tokio::test]
async fn pg_flow_scoping() {
    let Some(url) = pg_url() else {
        eprintln!("SKIP pg_flow_scoping: IRONCREW_TEST_PG_URL unset");
        return;
    };
    let prefix = "flow_";
    reset(&url, prefix).await;
    let store = PostgresStore::new(&url, prefix).await.unwrap();

    for (id, flow) in [("a1", "alpha"), ("a2", "alpha"), ("b1", "beta")] {
        store
            .save_run_intent(intent(id, flow, "2026-04-23T10:00:00Z", vec![]))
            .await
            .unwrap();
    }

    let alpha = ListRunsFilter { flow: Some("alpha".into()), ..Default::default() };
    let rows = store.list_runs_summary(&alpha, 10, 0).await.unwrap();
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().all(|r| r.flow == "alpha"));
    assert_eq!(store.count_runs(&alpha).await.unwrap(), 2);

    let beta = ListRunsFilter { flow: Some("beta".into()), ..Default::default() };
    assert_eq!(store.count_runs(&beta).await.unwrap(), 1);

    let ghost = ListRunsFilter { flow: Some("ghost".into()), ..Default::default() };
    assert_eq!(store.count_runs(&ghost).await.unwrap(), 0);
}

/// M5: jsonb `@>` tag containment matches exactly (not LIKE), even for a tag
/// containing wildcard/quote characters.
#[tokio::test]
async fn pg_tag_filter_exact_match() {
    let Some(url) = pg_url() else {
        eprintln!("SKIP pg_tag_filter_exact_match: IRONCREW_TEST_PG_URL unset");
        return;
    };
    let prefix = "tag_";
    reset(&url, prefix).await;
    let store = PostgresStore::new(&url, prefix).await.unwrap();

    let tricky = "a%_\"b".to_string();
    store
        .save_run_intent(intent("has-tricky", "f", "2026-04-23T10:00:00Z", vec![tricky.clone()]))
        .await
        .unwrap();
    store
        .save_run_intent(intent("has-plain", "f", "2026-04-23T10:01:00Z", vec!["plain".into()]))
        .await
        .unwrap();

    let exact = ListRunsFilter { tag: Some(tricky), ..Default::default() };
    let rows = store.list_runs_summary(&exact, 10, 0).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].run_id, "has-tricky");

    // A wildcard-y query must not match via LIKE semantics.
    let wildcard = ListRunsFilter { tag: Some("a%".into()), ..Default::default() };
    assert_eq!(store.count_runs(&wildcard).await.unwrap(), 0);
}

#[tokio::test]
async fn pg_reconcile_abandoned() {
    let Some(url) = pg_url() else {
        eprintln!("SKIP pg_reconcile_abandoned: IRONCREW_TEST_PG_URL unset");
        return;
    };
    let prefix = "rec_";
    reset(&url, prefix).await;
    let store = PostgresStore::new(&url, prefix).await.unwrap();

    store.save_run_intent(intent("r1", "f", "2026-04-23T10:00:00Z", vec![])).await.unwrap();
    store.save_run_intent(intent("r2", "f", "2026-04-23T10:01:00Z", vec![])).await.unwrap();

    let n = store.reconcile_abandoned_runs("2026-04-23T10:05:00Z").await.unwrap();
    assert_eq!(n, 2);
    assert_eq!(store.get_run("r1").await.unwrap().status, RunStatus::Abandoned);
    assert_eq!(store.get_run("r1").await.unwrap().finished_at, "2026-04-23T10:05:00Z");

    // Idempotent — nothing left in Running.
    assert_eq!(store.reconcile_abandoned_runs("2026-04-23T10:06:00Z").await.unwrap(), 0);
}

#[tokio::test]
async fn pg_audit_roundtrip_and_filter() {
    let Some(url) = pg_url() else {
        eprintln!("SKIP pg_audit_roundtrip_and_filter: IRONCREW_TEST_PG_URL unset");
        return;
    };
    let prefix = "aud_";
    reset(&url, prefix).await;
    let store = PostgresStore::new(&url, prefix).await.unwrap();

    let rows = [
        ("flow-a", "flow.run.start", "alice", true),
        ("flow-a", "flow.run.delete", "alice", true),
        ("flow-b", "flow.run.start", "bob", false),
    ];
    for (i, (flow, action, actor, success)) in rows.iter().enumerate() {
        store
            .save_audit_event(&AuditEvent {
                id: String::new(),
                timestamp: format!("2026-05-21T10:0{i}:00Z"),
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

    // Filter by flow (text eq) and success (bool bind — the SqlParam::Bool path).
    let by_flow = AuditFilter { flow_path: Some("flow-a".into()), ..Default::default() };
    assert_eq!(store.count_audit_events(&by_flow).await.unwrap(), 2);

    let failures = AuditFilter { success: Some(false), ..Default::default() };
    let ev = store.list_audit_events(&failures, 10, 0).await.unwrap();
    assert_eq!(ev.len(), 1);
    assert_eq!(ev[0].action, "flow.run.start");
    assert!(!ev[0].success);
}

/// Exercises the additive migration: a pre-existing `runs` table WITHOUT the
/// `flow` column must gain it on `PostgresStore::new`, and remain usable.
#[tokio::test]
async fn pg_migration_adds_flow_column_to_old_schema() {
    let Some(url) = pg_url() else {
        eprintln!("SKIP pg_migration_adds_flow_column_to_old_schema: IRONCREW_TEST_PG_URL unset");
        return;
    };
    let prefix = "mig_";
    reset(&url, prefix).await;

    // Create an old-schema runs table (pre-`flow`) directly.
    let pool = sqlx::PgPool::connect(&url).await.unwrap();
    let create_old = format!(
        "CREATE TABLE {prefix}runs (
            run_id TEXT PRIMARY KEY,
            flow_name TEXT NOT NULL,
            status TEXT NOT NULL,
            started_at TEXT NOT NULL,
            finished_at TEXT NOT NULL,
            duration_ms BIGINT NOT NULL,
            task_results JSONB NOT NULL DEFAULT '[]',
            agent_count INTEGER NOT NULL,
            task_count INTEGER NOT NULL,
            total_tokens INTEGER DEFAULT 0,
            cached_tokens INTEGER DEFAULT 0,
            tags JSONB DEFAULT '[]',
            created_at TIMESTAMPTZ DEFAULT NOW()
        )"
    );
    sqlx::query(sqlx::AssertSqlSafe(create_old)).execute(&pool).await.unwrap();
    // Seed a legacy row that predates the flow column.
    let seed = format!(
        "INSERT INTO {prefix}runs (run_id, flow_name, status, started_at, finished_at, duration_ms, agent_count, task_count)
         VALUES ('legacy-1', 'old goal', 'success', '2026-01-01T00:00:00Z', '2026-01-01T00:00:01Z', 1000, 1, 1)"
    );
    sqlx::query(sqlx::AssertSqlSafe(seed)).execute(&pool).await.unwrap();

    // Confirm the column is absent before migration.
    let has_flow_before: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM information_schema.columns WHERE table_name = $1 AND column_name = 'flow')",
    )
    .bind(format!("{prefix}runs"))
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(!has_flow_before, "precondition: old schema has no flow column");
    pool.close().await;

    // Constructing the store runs the additive migration.
    let store = PostgresStore::new(&url, prefix).await.unwrap();

    // Legacy row is readable with an empty flow (default), and a new flow-scoped
    // run round-trips.
    let legacy = store.get_run("legacy-1").await.unwrap();
    assert_eq!(legacy.flow, "");
    store
        .save_run_intent(intent("new-1", "scoped", "2026-04-23T10:00:00Z", vec![]))
        .await
        .unwrap();
    let scoped = ListRunsFilter { flow: Some("scoped".into()), ..Default::default() };
    let rows = store.list_runs_summary(&scoped, 10, 0).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].run_id, "new-1");
    // The pre-migration legacy row is not visible under a real flow scope.
    assert_eq!(store.count_runs(&scoped).await.unwrap(), 1);
}
