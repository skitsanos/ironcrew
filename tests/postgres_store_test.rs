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
use ironcrew::engine::idempotency::{
    CONVERSATION_MESSAGE_OPERATION, IdempotencyClaim, IdempotencyClaimOutcome,
    IdempotencyCompletion, IdempotencyLimits, IdempotencyLookup, IdempotencyQuotaResource,
    IdempotencyQuotaScope, IdempotencyState, PrincipalId, RUN_OPERATION, RunCancellationRequest,
    RunFenceHeartbeat,
};
use ironcrew::engine::postgres_store::PostgresStore;
use ironcrew::engine::run_history::{
    ListRunsFilter, RunCompletion, RunIntent, RunStatus, RunTransition,
};
use ironcrew::engine::sessions::{ConversationRecord, DialogStateRecord};
use ironcrew::engine::store::{RunLeaseConfig, StateStore};
use ironcrew::engine::task::TaskResult;
use ironcrew::llm::provider::ChatMessage;
use ironcrew::lua::dialog::DialogTurn;
use std::time::Duration;

fn pg_url() -> Option<String> {
    std::env::var("IRONCREW_TEST_PG_URL")
        .ok()
        .filter(|s| !s.is_empty())
}

/// Drop this test's prefixed tables so it starts from a clean schema even when
/// the database is reused across runs.
async fn reset(url: &str, prefix: &str) {
    let pool = sqlx::PgPool::connect(url).await.expect("connect for reset");
    for t in [
        "runs",
        "conversations",
        "dialogs",
        "audit_events",
        "idempotency",
        "idempotency_accounting",
    ] {
        let sql = format!("DROP TABLE IF EXISTS {prefix}{t} CASCADE");
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .execute(&pool)
            .await
            .expect("drop table");
    }
    let function = format!("DROP FUNCTION IF EXISTS {prefix}idempotency_acct_fn() CASCADE");
    sqlx::query(sqlx::AssertSqlSafe(function))
        .execute(&pool)
        .await
        .expect("drop idempotency accounting function");
    pool.close().await;
}

async fn expire_run_lease(url: &str, prefix: &str, run_id: &str) {
    let pool = sqlx::PgPool::connect(url)
        .await
        .expect("connect to expire run");
    let sql = format!(
        "UPDATE {prefix}runs SET lease_expires_at = to_char(\
             (clock_timestamp() - interval '1 second') AT TIME ZONE 'UTC', \
             'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"'\
         ) WHERE run_id = $1"
    );
    sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(run_id)
        .execute(&pool)
        .await
        .expect("expire run lease");
    pool.close().await;
}

async fn expire_idempotency_lease(url: &str, prefix: &str, key_hash: &str) {
    let pool = sqlx::PgPool::connect(url)
        .await
        .expect("connect to expire idempotency lease");
    let sql = format!(
        "UPDATE {prefix}idempotency SET lease_expires_at = to_char(\
             (clock_timestamp() - interval '1 second') AT TIME ZONE 'UTC', \
             'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"'\
         ) WHERE key_hash = $1"
    );
    sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(key_hash)
        .execute(&pool)
        .await
        .expect("expire idempotency lease");
    pool.close().await;
}

async fn expire_idempotency_retention(url: &str, prefix: &str, key_hash: &str) {
    let pool = sqlx::PgPool::connect(url)
        .await
        .expect("connect to expire idempotency retention");
    let sql = format!(
        "UPDATE {prefix}idempotency SET expires_at = to_char(\
             (clock_timestamp() - interval '1 second') AT TIME ZONE 'UTC', \
             'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"'\
         ) WHERE key_hash = $1"
    );
    sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(key_hash)
        .execute(&pool)
        .await
        .expect("expire idempotency retention");
    pool.close().await;
}

async fn age_idempotency_completion(url: &str, prefix: &str, key_hash: &str, age_seconds: u64) {
    let pool = sqlx::PgPool::connect(url)
        .await
        .expect("connect to age idempotency completion");
    let sql = format!(
        "UPDATE {prefix}idempotency SET completed_at = to_char(\
             (clock_timestamp() - $2::bigint * interval '1 second') AT TIME ZONE 'UTC', \
             'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"'\
         ), updated_at = to_char(\
             (clock_timestamp() - $2::bigint * interval '1 second') AT TIME ZONE 'UTC', \
             'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"'\
         ) WHERE key_hash = $1"
    );
    sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(key_hash)
        .bind(i64::try_from(age_seconds).expect("completion age fits PostgreSQL"))
        .execute(&pool)
        .await
        .expect("age idempotency completion");
    pool.close().await;
}

async fn postgres_now(url: &str) -> chrono::DateTime<chrono::Utc> {
    let pool = sqlx::PgPool::connect(url)
        .await
        .expect("connect to read PostgreSQL clock");
    let now: String = sqlx::query_scalar(
        "SELECT to_char(\
             clock_timestamp() AT TIME ZONE 'UTC', \
             'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"'\
         )",
    )
    .fetch_one(&pool)
    .await
    .expect("read PostgreSQL clock");
    pool.close().await;
    chrono::DateTime::parse_from_rfc3339(&now)
        .expect("parse PostgreSQL clock")
        .with_timezone(&chrono::Utc)
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

fn digest(byte: char) -> String {
    byte.to_string().repeat(64)
}

fn idempotency_limits(
    max_records: usize,
    max_response_bytes: usize,
    prune_batch: usize,
) -> IdempotencyLimits {
    IdempotencyLimits {
        global_max_records: max_records,
        principal_max_records: max_records,
        principal_max_in_flight: max_records,
        global_max_response_bytes: max_response_bytes,
        principal_max_response_bytes: max_response_bytes,
        prune_batch: prune_batch.max(1),
    }
}

fn default_idempotency_limits() -> IdempotencyLimits {
    idempotency_limits(100, usize::MAX, 10)
}

fn idempotency_claim(
    key: char,
    fingerprint: char,
    operation: &str,
    resource_id: &str,
    exclusive_scope: Option<&str>,
    base_revision: Option<u64>,
    lease_expires_at: &str,
) -> IdempotencyClaim {
    IdempotencyClaim {
        key_hash: digest(key),
        principal_id: PrincipalId::legacy(),
        recovery_key_hash: None,
        request_fingerprint: digest(fingerprint),
        operation: operation.into(),
        scope: "flow-a".into(),
        resource_id: resource_id.into(),
        exclusive_scope: exclusive_scope.map(str::to_string),
        attempt_id: format!("attempt-{key}"),
        owner_instance_id: "owner-a".into(),
        base_revision,
        response_status: None,
        response_body: None,
        max_total_response_bytes: usize::MAX,
        lease_expires_at: lease_expires_at.into(),
        created_at: "2026-07-19T12:00:00Z".into(),
        ttl_seconds: 86_400,
    }
}

fn idempotency_completion(claim: &IdempotencyClaim, body: Option<&str>) -> IdempotencyCompletion {
    IdempotencyCompletion {
        key_hash: claim.key_hash.clone(),
        principal_id: claim.principal_id.clone(),
        request_fingerprint: claim.request_fingerprint.clone(),
        attempt_id: claim.attempt_id.clone(),
        owner_instance_id: claim.owner_instance_id.clone(),
        response_status: 200,
        response_body: body.map(str::to_string),
        completed_at: "2026-07-19T12:01:00Z".into(),
        expires_at: "2026-07-20T12:01:00Z".into(),
    }
}

#[tokio::test]
async fn pg_rejects_stale_session_snapshots() {
    let Some(url) = pg_url() else {
        eprintln!("SKIP pg_rejects_stale_session_snapshots: IRONCREW_TEST_PG_URL unset");
        return;
    };
    let prefix = "session_cas_";
    reset(&url, prefix).await;
    let store = PostgresStore::new(&url, prefix).await.unwrap();

    let mut conversation = ConversationRecord {
        id: "shared".into(),
        flow_name: "chat".into(),
        flow_path: Some("flow-a".into()),
        agent_name: "assistant".into(),
        messages: vec![ChatMessage::system("system")],
        created_at: "2026-07-18T10:00:00Z".into(),
        updated_at: "2026-07-18T10:00:00Z".into(),
        revision: 0,
    };
    conversation.revision = store.save_conversation(&conversation).await.unwrap();
    let stale_conversation = conversation.clone();
    conversation.messages.push(ChatMessage::user("winner"));
    conversation.revision = store.save_conversation(&conversation).await.unwrap();
    assert!(matches!(
        store.save_conversation(&stale_conversation).await,
        Err(ironcrew::utils::error::IronCrewError::Conflict(_))
    ));
    assert_eq!(
        store
            .get_conversation(Some("flow-a"), "shared")
            .await
            .unwrap()
            .unwrap()
            .revision,
        2
    );

    let mut dialog = DialogStateRecord {
        id: "shared-dialog".into(),
        flow_name: "dialog".into(),
        flow_path: Some("flow-a".into()),
        agent_names: vec!["alice".into(), "bob".into()],
        starter: "start".into(),
        transcript: vec![DialogTurn {
            index: 0,
            speaker_index: 0,
            agent_name: "alice".into(),
            content: "hello".into(),
            reasoning: None,
        }],
        next_index: 1,
        stopped: false,
        stop_reason: None,
        created_at: "2026-07-18T10:00:00Z".into(),
        updated_at: "2026-07-18T10:00:00Z".into(),
        revision: 0,
    };
    dialog.revision = store.save_dialog_state(&dialog).await.unwrap();
    let stale_dialog = dialog.clone();
    dialog.stopped = true;
    dialog.revision = store.save_dialog_state(&dialog).await.unwrap();
    assert!(matches!(
        store.save_dialog_state(&stale_dialog).await,
        Err(ironcrew::utils::error::IronCrewError::Conflict(_))
    ));
    assert_eq!(
        store
            .get_dialog_state(Some("flow-a"), "shared-dialog")
            .await
            .unwrap()
            .unwrap()
            .revision,
        2
    );
}

#[tokio::test]
async fn pg_migrates_legacy_sessions_to_revision_guarded_updates() {
    let Some(url) = pg_url() else {
        eprintln!(
            "SKIP pg_migrates_legacy_sessions_to_revision_guarded_updates: IRONCREW_TEST_PG_URL unset"
        );
        return;
    };
    let prefix = "session_mig_";
    reset(&url, prefix).await;
    let pool = sqlx::PgPool::connect(&url).await.unwrap();
    let legacy_schema = format!(
        "CREATE TABLE {prefix}conversations (
            id TEXT PRIMARY KEY, flow_name TEXT NOT NULL, agent_name TEXT NOT NULL,
            messages JSONB NOT NULL DEFAULT '[]', created_at TEXT NOT NULL, updated_at TEXT NOT NULL
         );
         CREATE TABLE {prefix}dialogs (
            id TEXT PRIMARY KEY, flow_name TEXT NOT NULL, agent_names JSONB NOT NULL DEFAULT '[]',
            starter TEXT NOT NULL, transcript JSONB NOT NULL DEFAULT '[]', next_index INTEGER NOT NULL,
            stopped BOOLEAN NOT NULL DEFAULT FALSE, stop_reason TEXT, created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL
         );
         INSERT INTO {prefix}conversations
            (id, flow_name, agent_name, messages, created_at, updated_at)
            VALUES ('legacy-chat', 'chat', 'assistant', '[]', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z');
         INSERT INTO {prefix}dialogs
            (id, flow_name, agent_names, starter, transcript, next_index, created_at, updated_at)
            VALUES ('legacy-dialog', 'dialog', '[\"alice\",\"bob\"]', 'start', '[]', 0,
                    '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')"
    );
    sqlx::raw_sql(sqlx::AssertSqlSafe(legacy_schema))
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;

    let store = PostgresStore::new(&url, prefix).await.unwrap();
    let mut conversation = store
        .get_conversation(None, "legacy-chat")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(conversation.revision, 0);
    conversation.revision = store.save_conversation(&conversation).await.unwrap();
    assert_eq!(conversation.revision, 1);

    let mut dialog = store
        .get_dialog_state(None, "legacy-dialog")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(dialog.revision, 0);
    dialog.revision = store.save_dialog_state(&dialog).await.unwrap();
    assert_eq!(dialog.revision, 1);
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
        .save_run_intent(intent(
            "pg-1",
            "demo",
            "2026-04-23T10:00:00Z",
            vec!["dev".into()],
        ))
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

    // A racing finalizer observes the winner without overwriting it.
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
    assert_eq!(
        again.unwrap(),
        RunTransition::AlreadyTerminal(RunStatus::Success)
    );
    assert_eq!(
        store.get_run(&run_id).await.unwrap().status,
        RunStatus::Success
    );
}

#[tokio::test]
async fn pg_persists_abort_once() {
    let Some(url) = pg_url() else {
        eprintln!("SKIP pg_persists_abort_once: IRONCREW_TEST_PG_URL unset");
        return;
    };
    let prefix = "abort_";
    reset(&url, prefix).await;
    let store = PostgresStore::new(&url, prefix).await.unwrap();
    store
        .save_run_intent(intent("abort-pg", "demo", "2026-07-18T10:00:00Z", vec![]))
        .await
        .unwrap();
    let completion = RunCompletion {
        status: RunStatus::Aborted,
        finished_at: "2026-07-18T10:00:01Z".into(),
        duration_ms: 1_000,
        task_results: vec![],
        total_tokens: 0,
        cached_tokens: 0,
    };
    assert_eq!(
        store
            .update_run_completion("abort-pg", completion.clone())
            .await
            .unwrap(),
        RunTransition::Applied
    );
    assert_eq!(
        store
            .update_run_completion("abort-pg", completion)
            .await
            .unwrap(),
        RunTransition::AlreadyTerminal(RunStatus::Aborted)
    );
    assert_eq!(
        store.get_run("abort-pg").await.unwrap().status,
        RunStatus::Aborted
    );
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

    let alpha = ListRunsFilter {
        flow: Some("alpha".into()),
        ..Default::default()
    };
    let rows = store.list_runs_summary(&alpha, 10, 0).await.unwrap();
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().all(|r| r.flow == "alpha"));
    assert_eq!(store.count_runs(&alpha).await.unwrap(), 2);

    let beta = ListRunsFilter {
        flow: Some("beta".into()),
        ..Default::default()
    };
    assert_eq!(store.count_runs(&beta).await.unwrap(), 1);

    let ghost = ListRunsFilter {
        flow: Some("ghost".into()),
        ..Default::default()
    };
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
        .save_run_intent(intent(
            "has-tricky",
            "f",
            "2026-04-23T10:00:00Z",
            vec![tricky.clone()],
        ))
        .await
        .unwrap();
    store
        .save_run_intent(intent(
            "has-plain",
            "f",
            "2026-04-23T10:01:00Z",
            vec!["plain".into()],
        ))
        .await
        .unwrap();

    let exact = ListRunsFilter {
        tag: Some(tricky),
        ..Default::default()
    };
    let rows = store.list_runs_summary(&exact, 10, 0).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].run_id, "has-tricky");

    // A wildcard-y query must not match via LIKE semantics.
    let wildcard = ListRunsFilter {
        tag: Some("a%".into()),
        ..Default::default()
    };
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

    store
        .save_run_intent(intent("r1", "f", "2026-04-23T10:00:00Z", vec![]))
        .await
        .unwrap();
    store
        .save_run_intent(intent("r2", "f", "2026-04-23T10:01:00Z", vec![]))
        .await
        .unwrap();
    expire_run_lease(&url, prefix, "r1").await;
    expire_run_lease(&url, prefix, "r2").await;

    let n = store
        .reconcile_abandoned_runs("1900-04-23T10:05:00Z")
        .await
        .unwrap();
    assert_eq!(n, 2);
    assert_eq!(
        store.get_run("r1").await.unwrap().status,
        RunStatus::Abandoned
    );
    let finished_at = store.get_run("r1").await.unwrap().finished_at;
    assert!(chrono::DateTime::parse_from_rfc3339(&finished_at).is_ok());
    assert_ne!(finished_at, "1900-04-23T10:05:00Z");

    // Idempotent — nothing left in Running.
    assert_eq!(
        store
            .reconcile_abandoned_runs("9999-04-23T10:06:00Z")
            .await
            .unwrap(),
        0
    );
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
    let by_flow = AuditFilter {
        flow_path: Some("flow-a".into()),
        ..Default::default()
    };
    assert_eq!(store.count_audit_events(&by_flow).await.unwrap(), 2);

    let failures = AuditFilter {
        success: Some(false),
        ..Default::default()
    };
    let ev = store.list_audit_events(&failures, 10, 0).await.unwrap();
    assert_eq!(ev.len(), 1);
    assert_eq!(ev[0].action, "flow.run.start");
    assert!(!ev[0].success);
}

/// Exercises additive migrations: a pre-existing `runs` table without flow or
/// lease columns must gain them on `PostgresStore::new` and remain usable.
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
    sqlx::query(sqlx::AssertSqlSafe(create_old))
        .execute(&pool)
        .await
        .unwrap();
    // Seed a legacy row that predates the flow column.
    let seed = format!(
        "INSERT INTO {prefix}runs (run_id, flow_name, status, started_at, finished_at, duration_ms, agent_count, task_count)
         VALUES ('legacy-1', 'old goal', 'success', '2026-01-01T00:00:00Z', '2026-01-01T00:00:01Z', 1000, 1, 1)"
    );
    sqlx::query(sqlx::AssertSqlSafe(seed))
        .execute(&pool)
        .await
        .unwrap();

    // Confirm the column is absent before migration.
    let has_flow_before: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM information_schema.columns WHERE table_name = $1 AND column_name = 'flow')",
    )
    .bind(format!("{prefix}runs"))
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(
        !has_flow_before,
        "precondition: old schema has no flow column"
    );
    pool.close().await;

    // Constructing the store runs the additive migration.
    let store = PostgresStore::new(&url, prefix).await.unwrap();

    // Legacy row is readable with an empty flow (default), and a new flow-scoped
    // run round-trips.
    let legacy = store.get_run("legacy-1").await.unwrap();
    assert_eq!(legacy.flow, "");
    assert_eq!(legacy.owner_instance_id, "");
    assert_eq!(legacy.lease_expires_at, "");
    store
        .save_run_intent(intent("new-1", "scoped", "2026-04-23T10:00:00Z", vec![]))
        .await
        .unwrap();
    let new_run = store.get_run("new-1").await.unwrap();
    assert!(!new_run.owner_instance_id.is_empty());
    assert!(!new_run.lease_expires_at.is_empty());
    let scoped = ListRunsFilter {
        flow: Some("scoped".into()),
        ..Default::default()
    };
    let rows = store.list_runs_summary(&scoped, 10, 0).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].run_id, "new-1");
    // The pre-migration legacy row is not visible under a real flow scope.
    assert_eq!(store.count_runs(&scoped).await.unwrap(), 1);

    // A deployment upgraded from before request idempotency also receives the
    // complete ledger schema and compact, non-truncated indexes atomically.
    let pool = sqlx::PgPool::connect(&url).await.unwrap();
    let idempotency_columns: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM information_schema.columns \
         WHERE table_schema = current_schema() AND table_name = $1",
    )
    .bind(format!("{prefix}idempotency"))
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(idempotency_columns, 20);
    let idempotency_indexes: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM pg_indexes \
         WHERE schemaname = current_schema() AND tablename = $1",
    )
    .bind(format!("{prefix}idempotency"))
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(idempotency_indexes, 4, "primary key plus three indexes");
    let accounting_columns: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM information_schema.columns \
         WHERE table_schema = current_schema() AND table_name = $1",
    )
    .bind(format!("{prefix}idempotency_accounting"))
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(accounting_columns, 6);
    let trigger_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM pg_trigger AS trg \
         JOIN pg_class AS table_class ON table_class.oid = trg.tgrelid \
         WHERE table_class.relname = $1 AND NOT trg.tgisinternal",
    )
    .bind(format!("{prefix}idempotency"))
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(trigger_count, 1);
    pool.close().await;
}

#[tokio::test]
async fn pg_migrates_legacy_principals_and_reconciles_accounting() {
    let Some(url) = pg_url() else {
        eprintln!(
            "SKIP pg_migrates_legacy_principals_and_reconciles_accounting: IRONCREW_TEST_PG_URL unset"
        );
        return;
    };
    let prefix = "idem_account_mig_";
    reset(&url, prefix).await;
    let pool = sqlx::PgPool::connect(&url).await.unwrap();
    let create = format!(
        "CREATE TABLE {prefix}idempotency (
            key_hash TEXT PRIMARY KEY,
            request_fingerprint TEXT NOT NULL,
            operation TEXT NOT NULL,
            scope TEXT NOT NULL,
            resource_id TEXT NOT NULL,
            exclusive_scope TEXT,
            attempt_id TEXT NOT NULL,
            owner_instance_id TEXT NOT NULL,
            base_revision BIGINT,
            state TEXT NOT NULL,
            response_status INTEGER,
            response_body TEXT,
            lease_expires_at TEXT NOT NULL,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            completed_at TEXT,
            expires_at TEXT,
            ttl_seconds BIGINT NOT NULL
        )"
    );
    sqlx::query(sqlx::AssertSqlSafe(create))
        .execute(&pool)
        .await
        .unwrap();
    let insert = format!(
        "INSERT INTO {prefix}idempotency \
         (key_hash, request_fingerprint, operation, scope, resource_id, exclusive_scope, \
          attempt_id, owner_instance_id, base_revision, state, response_status, response_body, \
          lease_expires_at, created_at, updated_at, completed_at, expires_at, ttl_seconds) \
         VALUES \
         ($1, $2, 'flow.run', 'flow-a', 'legacy-run', NULL, 'legacy-a', 'legacy-owner', NULL, \
          'claimed', 202, 'abc', '9999-01-01T00:00:00Z', '2026-01-01T00:00:00Z', \
          '2026-01-01T00:00:00Z', NULL, NULL, 86400), \
         ($3, $4, 'conversation.message', 'flow-a', 'legacy-chat', NULL, 'legacy-b', \
          'legacy-owner', 0, 'completed', 200, 'hello', '', '2026-01-01T00:00:00Z', \
          '2026-01-01T00:01:00Z', '2026-01-01T00:01:00Z', '9999-01-01T00:00:00Z', 86400)"
    );
    sqlx::query(sqlx::AssertSqlSafe(insert))
        .bind(digest('7'))
        .bind(digest('8'))
        .bind(digest('8'))
        .bind(digest('9'))
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;

    let store = PostgresStore::new(&url, prefix).await.unwrap();
    let usage = store
        .idempotency_usage(&PrincipalId::legacy(), default_idempotency_limits())
        .await
        .unwrap();
    assert_eq!(usage.global_records, 2);
    assert_eq!(usage.global_in_flight, 1);
    assert_eq!(usage.global_response_bytes, 8);
    assert_eq!(usage.principal_records, 2);
    assert_eq!(usage.principal_in_flight, 1);
    assert_eq!(usage.principal_response_bytes, 8);
    assert_eq!(usage.principal_count, 1);

    let pool = sqlx::PgPool::connect(&url).await.unwrap();
    let migrated: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT COUNT(*) FROM {prefix}idempotency WHERE principal_id = $1"
    )))
    .bind(PrincipalId::legacy().as_str())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(migrated, 2);
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE {prefix}idempotency_accounting \
         SET record_count = 0, in_flight_count = 0, response_bytes = 0"
    )))
    .execute(&pool)
    .await
    .unwrap();
    pool.close().await;

    let repaired = PostgresStore::new(&url, prefix).await.unwrap();
    let usage = repaired
        .idempotency_usage(&PrincipalId::legacy(), default_idempotency_limits())
        .await
        .unwrap();
    assert_eq!(usage.global_records, 2);
    assert_eq!(usage.global_in_flight, 1);
    assert_eq!(usage.global_response_bytes, 8);
    assert_eq!(usage.principal_records, 2);
    assert_eq!(usage.principal_in_flight, 1);
    assert_eq!(usage.principal_response_bytes, 8);
}

#[tokio::test]
async fn pg_update_run_status_waiting_round_trip() {
    let Some(url) = pg_url() else {
        eprintln!("SKIP pg_update_run_status_waiting_round_trip: IRONCREW_TEST_PG_URL unset");
        return;
    };
    let prefix = "wfi_";
    reset(&url, prefix).await;
    let store = PostgresStore::new(&url, prefix).await.unwrap();

    store
        .save_run_intent(intent("wfi-1", "demo", "2026-07-07T10:00:00Z", vec![]))
        .await
        .unwrap();

    // Running -> WaitingForInput -> back (ask_human suspend/resume).
    store
        .update_run_status("wfi-1", RunStatus::WaitingForInput)
        .await
        .unwrap();
    assert_eq!(
        store.get_run("wfi-1").await.unwrap().status,
        RunStatus::WaitingForInput
    );

    // Completion is accepted while WaitingForInput.
    store
        .update_run_completion(
            "wfi-1",
            RunCompletion {
                status: RunStatus::Failed,
                finished_at: "2026-07-07T10:01:00Z".into(),
                duration_ms: 60_000,
                task_results: Vec::new(),
                total_tokens: 0,
                cached_tokens: 0,
            },
        )
        .await
        .unwrap();

    // Terminal records reject flips; unknown ids error.
    assert!(
        store
            .update_run_status("wfi-1", RunStatus::Running)
            .await
            .is_err()
    );
    assert!(
        store
            .update_run_status("missing", RunStatus::Running)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn pg_reconcile_sweeps_waiting_for_input() {
    let Some(url) = pg_url() else {
        eprintln!("SKIP pg_reconcile_sweeps_waiting_for_input: IRONCREW_TEST_PG_URL unset");
        return;
    };
    let prefix = "wfirec_";
    reset(&url, prefix).await;
    let store = PostgresStore::new(&url, prefix).await.unwrap();

    store
        .save_run_intent(intent("wfi-2", "demo", "2026-07-07T10:00:00Z", vec![]))
        .await
        .unwrap();
    store
        .update_run_status("wfi-2", RunStatus::WaitingForInput)
        .await
        .unwrap();
    expire_run_lease(&url, prefix, "wfi-2").await;

    let count = store
        .reconcile_abandoned_runs("1900-07-07T11:00:00Z")
        .await
        .unwrap();
    assert_eq!(count, 1);
    let r = store.get_run("wfi-2").await.unwrap();
    assert_eq!(r.status, RunStatus::Abandoned);
    assert!(chrono::DateTime::parse_from_rfc3339(&r.finished_at).is_ok());
    assert_ne!(r.finished_at, "1900-07-07T11:00:00Z");
}

#[tokio::test]
async fn pg_multi_instance_lease_prevents_live_run_sweep() {
    let Some(url) = pg_url() else {
        eprintln!(
            "SKIP pg_multi_instance_lease_prevents_live_run_sweep: IRONCREW_TEST_PG_URL unset"
        );
        return;
    };
    let prefix = "lease_";
    reset(&url, prefix).await;
    let owner_a = PostgresStore::new_with_lease_config(
        &url,
        prefix,
        RunLeaseConfig::new("owner-a", Duration::from_secs(60)).unwrap(),
    )
    .await
    .unwrap();
    let owner_b = PostgresStore::new_with_lease_config(
        &url,
        prefix,
        RunLeaseConfig::new("owner-b", Duration::from_secs(60)).unwrap(),
    )
    .await
    .unwrap();

    owner_a.health_check().await.unwrap();

    owner_a
        .save_run_intent(intent("owned-run", "demo", "2026-07-18T10:00:00Z", vec![]))
        .await
        .unwrap();
    assert_eq!(owner_a.heartbeat_owned_runs().await.unwrap(), 1);

    let fresh = owner_a.get_run("owned-run").await.unwrap();
    assert_eq!(fresh.owner_instance_id, "owner-a");
    assert_eq!(
        owner_b
            .reconcile_abandoned_runs("9999-07-18T10:00:00Z")
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        owner_b.get_run("owned-run").await.unwrap().status,
        RunStatus::Running
    );

    expire_run_lease(&url, prefix, "owned-run").await;
    assert_eq!(
        owner_b
            .reconcile_abandoned_runs("1900-07-18T10:00:00Z")
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        owner_a.get_run("owned-run").await.unwrap().status,
        RunStatus::Abandoned
    );

    let late_completion = owner_a
        .update_run_completion(
            "owned-run",
            RunCompletion {
                status: RunStatus::Success,
                finished_at: "9999-07-18T10:00:00Z".into(),
                duration_ms: 1,
                task_results: vec![],
                total_tokens: 0,
                cached_tokens: 0,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        late_completion,
        RunTransition::AlreadyTerminal(RunStatus::Abandoned)
    );
    assert_eq!(
        owner_a.get_run("owned-run").await.unwrap().status,
        RunStatus::Abandoned
    );
}

#[tokio::test]
async fn pg_two_stores_claim_once_and_fence_stale_attempts() {
    let Some(url) = pg_url() else {
        eprintln!(
            "SKIP pg_two_stores_claim_once_and_fence_stale_attempts: IRONCREW_TEST_PG_URL unset"
        );
        return;
    };
    let prefix = "idem_claim_";
    reset(&url, prefix).await;
    let store_a = PostgresStore::new(&url, prefix).await.unwrap();
    let store_b = PostgresStore::new(&url, prefix).await.unwrap();
    let claim = idempotency_claim(
        'a',
        'b',
        CONVERSATION_MESSAGE_OPERATION,
        "chat-1",
        Some("conversation:flow-a:chat-1"),
        Some(0),
        "2026-07-19T12:10:00Z",
    );

    let (left, right) = tokio::join!(
        store_a.claim_idempotency_with_limits(claim.clone(), default_idempotency_limits()),
        store_b.claim_idempotency_with_limits(claim.clone(), default_idempotency_limits())
    );
    let outcomes = [left.unwrap(), right.unwrap()];
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| matches!(outcome, IdempotencyClaimOutcome::Claimed(_)))
            .count(),
        1
    );
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| matches!(outcome, IdempotencyClaimOutcome::InProgress(_)))
            .count(),
        1
    );

    assert!(matches!(
        store_b
            .heartbeat_idempotency(&claim.key_hash, "stale-attempt", "2026-07-19T12:11:00Z")
            .await,
        Err(ironcrew::utils::error::IronCrewError::Conflict(_))
    ));
    let mut stale_completion = idempotency_completion(&claim, Some("{\"ok\":true}"));
    stale_completion.attempt_id = "stale-attempt".into();
    assert!(matches!(
        store_b
            .complete_idempotency_with_limits(stale_completion, idempotency_limits(100, 1024, 10))
            .await,
        Err(ironcrew::utils::error::IronCrewError::Conflict(_))
    ));

    let completion = idempotency_completion(&claim, Some("{\"ok\":true}"));
    let completed = store_a
        .complete_idempotency_with_limits(completion.clone(), idempotency_limits(100, 1024, 10))
        .await
        .unwrap();
    assert!(completed.replayable);
    assert!(!completed.already_completed);
    let repeated = store_b
        .complete_idempotency_with_limits(completion, idempotency_limits(100, 1024, 10))
        .await
        .unwrap();
    assert!(repeated.replayable);
    assert!(repeated.already_completed);
    match store_b
        .lookup_idempotency(
            &claim.key_hash,
            &claim.request_fingerprint,
            "2026-07-19T12:02:00Z",
        )
        .await
        .unwrap()
    {
        IdempotencyLookup::Replay(record) => {
            assert_eq!(record.response_body.as_deref(), Some("{\"ok\":true}"));
        }
        other => panic!("expected replay, got {other:?}"),
    }
}

#[tokio::test]
async fn pg_key_advisory_locks_do_not_serialize_unrelated_heartbeats() {
    let Some(url) = pg_url() else {
        eprintln!(
            "SKIP pg_key_advisory_locks_do_not_serialize_unrelated_heartbeats: IRONCREW_TEST_PG_URL unset"
        );
        return;
    };
    let prefix = "idem_key_lock_";
    reset(&url, prefix).await;
    let store = PostgresStore::new(&url, prefix).await.unwrap();
    let first = idempotency_claim(
        '1',
        '2',
        CONVERSATION_MESSAGE_OPERATION,
        "lock-a",
        Some("conversation:flow-a:lock-a"),
        Some(0),
        "9999-01-01T00:00:00Z",
    );
    let second = idempotency_claim(
        '2',
        '3',
        CONVERSATION_MESSAGE_OPERATION,
        "lock-b",
        Some("conversation:flow-a:lock-b"),
        Some(0),
        "9999-01-01T00:00:00Z",
    );
    assert!(matches!(
        store
            .claim_idempotency_with_limits(first.clone(), default_idempotency_limits())
            .await
            .unwrap(),
        IdempotencyClaimOutcome::Claimed(_)
    ));
    assert!(matches!(
        store
            .claim_idempotency_with_limits(second.clone(), default_idempotency_limits())
            .await
            .unwrap(),
        IdempotencyClaimOutcome::Claimed(_)
    ));

    let pool = sqlx::PgPool::connect(&url).await.unwrap();
    let mut blocker = pool.begin().await.unwrap();
    let lock_name = format!(
        "ironcrew:{prefix}idempotency:idempotency-key:64:{}",
        first.key_hash
    );
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(lock_name)
        .execute(&mut *blocker)
        .await
        .unwrap();

    let unrelated = tokio::time::timeout(
        Duration::from_secs(2),
        store.heartbeat_idempotency(&second.key_hash, &second.attempt_id, "9999-01-01T00:00:00Z"),
    )
    .await
    .expect("an unrelated key must not wait for the held key lock")
    .unwrap();
    assert!(unrelated);
    assert!(
        tokio::time::timeout(
            Duration::from_millis(200),
            store
                .heartbeat_idempotency(&first.key_hash, &first.attempt_id, "9999-01-01T00:00:00Z",),
        )
        .await
        .is_err(),
        "the matching key must remain fenced by its advisory lock"
    );
    blocker.rollback().await.unwrap();
    assert!(
        store
            .heartbeat_idempotency(&first.key_hash, &first.attempt_id, "9999-01-01T00:00:00Z",)
            .await
            .unwrap()
    );
    pool.close().await;
}

#[tokio::test]
async fn pg_concurrent_principal_quota_is_exact_and_isolated() {
    let Some(url) = pg_url() else {
        eprintln!(
            "SKIP pg_concurrent_principal_quota_is_exact_and_isolated: IRONCREW_TEST_PG_URL unset"
        );
        return;
    };
    let prefix = "idem_principal_quota_";
    reset(&url, prefix).await;
    let store_a = PostgresStore::new(&url, prefix).await.unwrap();
    let store_b = PostgresStore::new(&url, prefix).await.unwrap();
    let principal = PrincipalId::from_label("tenant-a");
    let other_principal = PrincipalId::from_label("tenant-b");
    let mut first = idempotency_claim(
        '4',
        '5',
        RUN_OPERATION,
        "principal-run-a",
        None,
        None,
        "9999-01-01T00:00:00Z",
    );
    first.principal_id = principal.clone();
    let mut second = idempotency_claim(
        '5',
        '6',
        RUN_OPERATION,
        "principal-run-b",
        None,
        None,
        "9999-01-01T00:00:00Z",
    );
    second.principal_id = principal.clone();
    let limits = IdempotencyLimits {
        global_max_records: 10,
        principal_max_records: 1,
        principal_max_in_flight: 1,
        global_max_response_bytes: 1024,
        principal_max_response_bytes: 512,
        prune_batch: 1,
    };
    let (left, right) = tokio::join!(
        store_a.claim_idempotency_with_limits(first.clone(), limits),
        store_b.claim_idempotency_with_limits(second.clone(), limits)
    );
    let outcomes = [left.unwrap(), right.unwrap()];
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| matches!(outcome, IdempotencyClaimOutcome::Claimed(_)))
            .count(),
        1
    );
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| matches!(
                outcome,
                IdempotencyClaimOutcome::QuotaExceeded {
                    scope: IdempotencyQuotaScope::Principal,
                    resource: IdempotencyQuotaResource::Records,
                    retry_after_seconds: 1..,
                }
            ))
            .count(),
        1
    );

    let mut independent = idempotency_claim(
        '6',
        '7',
        RUN_OPERATION,
        "principal-run-c",
        None,
        None,
        "9999-01-01T00:00:00Z",
    );
    independent.principal_id = other_principal.clone();
    assert!(matches!(
        store_a
            .claim_idempotency_with_limits(independent, limits)
            .await
            .unwrap(),
        IdempotencyClaimOutcome::Claimed(_)
    ));

    let usage = store_a.idempotency_usage(&principal, limits).await.unwrap();
    assert_eq!(usage.global_records, 2);
    assert_eq!(usage.global_in_flight, 2);
    assert_eq!(usage.principal_records, 1);
    assert_eq!(usage.principal_in_flight, 1);
    assert_eq!(usage.principal_count, 2);
    assert_eq!(usage.max_principal_records, 1);
    assert_eq!(usage.principals_at_or_above_100_percent, 2);

    let winning_key = outcomes
        .iter()
        .find_map(|outcome| match outcome {
            IdempotencyClaimOutcome::Claimed(record) => Some(record.clone()),
            _ => None,
        })
        .unwrap();
    assert!(matches!(
        store_b
            .lookup_idempotency_for_principal(
                &other_principal,
                &winning_key.key_hash,
                &winning_key.request_fingerprint,
                "9999-01-01T00:00:00Z",
            )
            .await
            .unwrap(),
        IdempotencyLookup::Conflict
    ));
}

#[tokio::test]
async fn pg_idempotency_uses_database_clock_despite_client_skew() {
    let Some(url) = pg_url() else {
        eprintln!(
            "SKIP pg_idempotency_uses_database_clock_despite_client_skew: IRONCREW_TEST_PG_URL unset"
        );
        return;
    };
    let prefix = "idem_skew_";
    reset(&url, prefix).await;
    let store = PostgresStore::new_with_lease_config(
        &url,
        prefix,
        RunLeaseConfig::new("skew-owner", Duration::from_secs(60)).unwrap(),
    )
    .await
    .unwrap();

    let before_claim = postgres_now(&url).await;
    let mut claim = idempotency_claim(
        '4',
        '5',
        CONVERSATION_MESSAGE_OPERATION,
        "chat-skew",
        Some("conversation:flow-a:chat-skew"),
        Some(0),
        "1900-01-01T00:01:00Z",
    );
    claim.created_at = "1900-01-01T00:00:00Z".into();
    let stored = match store
        .claim_idempotency_with_limits(claim.clone(), default_idempotency_limits())
        .await
        .unwrap()
    {
        IdempotencyClaimOutcome::Claimed(record) => record,
        other => panic!("expected database-timed claim, got {other:?}"),
    };
    let after_claim = postgres_now(&url).await;
    let stored_created = chrono::DateTime::parse_from_rfc3339(&stored.created_at)
        .unwrap()
        .with_timezone(&chrono::Utc);
    let stored_deadline = chrono::DateTime::parse_from_rfc3339(&stored.lease_expires_at)
        .unwrap()
        .with_timezone(&chrono::Utc);
    assert!(stored.created_at.ends_with('Z'));
    assert!(stored.lease_expires_at.ends_with('Z'));
    assert!(stored_created >= before_claim && stored_created <= after_claim);
    assert_eq!(
        stored_deadline - stored_created,
        chrono::Duration::seconds(60)
    );

    let mut future_competitor = idempotency_claim(
        '5',
        '6',
        CONVERSATION_MESSAGE_OPERATION,
        "chat-skew",
        Some("conversation:flow-a:chat-skew"),
        Some(0),
        "9999-01-01T00:01:00Z",
    );
    future_competitor.created_at = "9999-01-01T00:00:00Z".into();
    assert!(matches!(
        store
            .claim_idempotency_with_limits(future_competitor, default_idempotency_limits())
            .await
            .unwrap(),
        IdempotencyClaimOutcome::Busy
    ));
    assert!(matches!(
        store
            .lookup_idempotency(
                &claim.key_hash,
                &claim.request_fingerprint,
                "9999-12-31T23:59:59Z",
            )
            .await
            .unwrap(),
        IdempotencyLookup::InProgress(_)
    ));

    assert!(
        store
            .heartbeat_idempotency(&claim.key_hash, &claim.attempt_id, "9999-12-31T23:59:59Z",)
            .await
            .unwrap()
    );
    let heartbeat_now = postgres_now(&url).await;
    let heartbeat_record = match store
        .lookup_idempotency(
            &claim.key_hash,
            &claim.request_fingerprint,
            "1900-01-01T00:00:00Z",
        )
        .await
        .unwrap()
    {
        IdempotencyLookup::InProgress(record) => record,
        other => panic!("expected live claim after skewed heartbeat, got {other:?}"),
    };
    let heartbeat_deadline =
        chrono::DateTime::parse_from_rfc3339(&heartbeat_record.lease_expires_at)
            .unwrap()
            .with_timezone(&chrono::Utc);
    assert!(heartbeat_deadline > heartbeat_now + chrono::Duration::seconds(55));
    assert!(heartbeat_deadline <= heartbeat_now + chrono::Duration::seconds(60));
    assert!(
        store
            .heartbeat_idempotency(&claim.key_hash, &claim.attempt_id, "1900-01-01T00:00:01Z",)
            .await
            .unwrap()
    );

    let completion = idempotency_completion(&claim, Some("{}"));
    store
        .complete_idempotency_with_limits(completion, idempotency_limits(100, 1024, 10))
        .await
        .unwrap();
    assert!(
        store
            .heartbeat_idempotency(&claim.key_hash, &claim.attempt_id, "1900-01-01T00:00:01Z",)
            .await
            .unwrap()
    );

    let indeterminate = idempotency_claim(
        '6',
        '7',
        RUN_OPERATION,
        "indeterminate-skew",
        None,
        None,
        "9999-01-01T00:01:00Z",
    );
    assert!(matches!(
        store
            .claim_idempotency_with_limits(indeterminate.clone(), default_idempotency_limits())
            .await
            .unwrap(),
        IdempotencyClaimOutcome::Claimed(_)
    ));
    assert!(
        store
            .mark_idempotency_indeterminate(
                &indeterminate.key_hash,
                &indeterminate.attempt_id,
                "1900-01-01T00:00:00Z",
                "9999-01-01T00:00:00Z",
            )
            .await
            .unwrap()
    );
    assert!(
        !store
            .heartbeat_idempotency(
                &indeterminate.key_hash,
                &indeterminate.attempt_id,
                "9999-01-01T00:00:00Z",
            )
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn pg_indeterminate_exclusive_scope_requires_recovery_after_grace() {
    let Some(url) = pg_url() else {
        eprintln!(
            "SKIP pg_indeterminate_exclusive_scope_requires_recovery_after_grace: IRONCREW_TEST_PG_URL unset"
        );
        return;
    };
    let prefix = "idem_hazard_";
    reset(&url, prefix).await;
    let store = PostgresStore::new_with_lease_config(
        &url,
        prefix,
        RunLeaseConfig::new("hazard-owner", Duration::from_secs(60)).unwrap(),
    )
    .await
    .unwrap();
    let scope = "conversation:flow-a:chat-hazard";
    let first = idempotency_claim(
        '8',
        '9',
        CONVERSATION_MESSAGE_OPERATION,
        "chat-hazard",
        Some(scope),
        Some(0),
        "9999-01-01T00:00:00Z",
    );
    assert!(matches!(
        store
            .claim_idempotency_with_limits(first.clone(), default_idempotency_limits())
            .await
            .unwrap(),
        IdempotencyClaimOutcome::Claimed(_)
    ));
    expire_idempotency_lease(&url, prefix, &first.key_hash).await;

    let second = idempotency_claim(
        '9',
        'a',
        CONVERSATION_MESSAGE_OPERATION,
        "chat-hazard",
        Some(scope),
        Some(0),
        "1900-01-01T00:00:00Z",
    );
    assert!(matches!(
        store
            .claim_idempotency_with_limits(second.clone(), default_idempotency_limits())
            .await
            .unwrap(),
        IdempotencyClaimOutcome::Busy
    ));
    assert!(matches!(
        store
            .lookup_idempotency(
                &first.key_hash,
                &first.request_fingerprint,
                "9999-01-01T00:00:00Z",
            )
            .await
            .unwrap(),
        IdempotencyLookup::Indeterminate(_)
    ));
    assert!(matches!(
        store
            .claim_idempotency_with_limits(second.clone(), default_idempotency_limits())
            .await
            .unwrap(),
        IdempotencyClaimOutcome::Busy
    ));

    let mut wrong_recovery = second.clone();
    wrong_recovery.recovery_key_hash = Some(digest('b'));
    assert!(matches!(
        store
            .claim_idempotency_with_limits(wrong_recovery, default_idempotency_limits())
            .await
            .unwrap(),
        IdempotencyClaimOutcome::Busy
    ));
    let mut acknowledged = second.clone();
    acknowledged.recovery_key_hash = Some(first.key_hash.clone());
    assert!(matches!(
        store
            .claim_idempotency_with_limits(acknowledged.clone(), default_idempotency_limits())
            .await
            .unwrap(),
        IdempotencyClaimOutcome::Busy
    ));
    match store
        .lookup_idempotency(
            &first.key_hash,
            &first.request_fingerprint,
            "1900-01-01T00:00:00Z",
        )
        .await
        .unwrap()
    {
        IdempotencyLookup::Indeterminate(record) => {
            assert_eq!(record.exclusive_scope.as_deref(), Some(scope));
        }
        other => panic!("expected unacknowledged hazard tombstone, got {other:?}"),
    }

    age_idempotency_completion(&url, prefix, &first.key_hash, 61).await;
    assert!(matches!(
        store
            .claim_idempotency_with_limits(
                acknowledged.clone(),
                idempotency_limits(1, usize::MAX, 10),
            )
            .await
            .unwrap(),
        IdempotencyClaimOutcome::QuotaExceeded {
            scope: IdempotencyQuotaScope::Global,
            resource: IdempotencyQuotaResource::Records,
            retry_after_seconds: 1..,
        }
    ));
    match store
        .lookup_idempotency(
            &first.key_hash,
            &first.request_fingerprint,
            "1900-01-01T00:00:00Z",
        )
        .await
        .unwrap()
    {
        IdempotencyLookup::Indeterminate(record) => {
            assert_eq!(
                record.exclusive_scope.as_deref(),
                Some(scope),
                "a quota-denied recovery must not consume the hazard binding"
            );
        }
        other => panic!("expected quota-preserved hazard tombstone, got {other:?}"),
    }
    let mut foreign_recovery = acknowledged.clone();
    foreign_recovery.principal_id = PrincipalId::from_label("foreign-hazard-principal");
    assert!(matches!(
        store
            .claim_idempotency_with_limits(foreign_recovery, default_idempotency_limits())
            .await
            .unwrap(),
        IdempotencyClaimOutcome::Busy
    ));
    assert!(matches!(
        store
            .claim_idempotency_with_limits(acknowledged, default_idempotency_limits())
            .await
            .unwrap(),
        IdempotencyClaimOutcome::Claimed(_)
    ));
    match store
        .lookup_idempotency(
            &first.key_hash,
            &first.request_fingerprint,
            "1900-01-01T00:00:00Z",
        )
        .await
        .unwrap()
    {
        IdempotencyLookup::Indeterminate(record) => assert!(record.exclusive_scope.is_none()),
        other => panic!("expected acknowledged hazard tombstone, got {other:?}"),
    }

    store
        .complete_idempotency_with_limits(
            idempotency_completion(&second, Some("{}")),
            idempotency_limits(100, 1024, 10),
        )
        .await
        .unwrap();
    let completed_is_ignored = idempotency_claim(
        'a',
        'b',
        CONVERSATION_MESSAGE_OPERATION,
        "chat-hazard",
        Some(scope),
        Some(0),
        "1900-01-01T00:00:00Z",
    );
    assert!(matches!(
        store
            .claim_idempotency_with_limits(completed_is_ignored, default_idempotency_limits())
            .await
            .unwrap(),
        IdempotencyClaimOutcome::Claimed(_)
    ));
}

#[tokio::test]
async fn pg_conversation_commit_is_atomic_and_blocks_unguarded_writes() {
    let Some(url) = pg_url() else {
        eprintln!(
            "SKIP pg_conversation_commit_is_atomic_and_blocks_unguarded_writes: IRONCREW_TEST_PG_URL unset"
        );
        return;
    };
    let prefix = "idem_chat_";
    reset(&url, prefix).await;
    let store = PostgresStore::new(&url, prefix).await.unwrap();
    let claim = idempotency_claim(
        'b',
        'c',
        CONVERSATION_MESSAGE_OPERATION,
        "chat-atomic",
        Some("conversation:flow-a:chat-atomic"),
        Some(0),
        "2026-07-19T12:10:00Z",
    );
    assert!(matches!(
        store
            .claim_idempotency_with_limits(claim.clone(), default_idempotency_limits())
            .await
            .unwrap(),
        IdempotencyClaimOutcome::Claimed(_)
    ));
    let conversation = ConversationRecord {
        id: "chat-atomic".into(),
        flow_name: "chat".into(),
        flow_path: Some("flow-a".into()),
        agent_name: "assistant".into(),
        messages: vec![ChatMessage::user("hello")],
        created_at: "2026-07-19T12:00:00Z".into(),
        updated_at: "2026-07-19T12:01:00Z".into(),
        revision: 0,
    };
    assert!(matches!(
        store.save_conversation(&conversation).await,
        Err(ironcrew::utils::error::IronCrewError::Conflict(_))
    ));
    assert!(matches!(
        store
            .delete_conversation(Some("flow-a"), "chat-atomic")
            .await,
        Err(ironcrew::utils::error::IronCrewError::Conflict(_))
    ));

    let completion = idempotency_completion(&claim, Some("{\"reply\":\"hi\"}"));
    let committed = store
        .commit_conversation_idempotency_with_limits(
            completion.clone(),
            &conversation,
            idempotency_limits(100, 1024, 10),
        )
        .await
        .unwrap();
    assert_eq!(committed.revision, 1);
    assert!(committed.replayable);
    assert!(!committed.already_completed);
    let persisted = store
        .get_conversation(Some("flow-a"), "chat-atomic")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(persisted.revision, 1);
    assert_eq!(persisted.messages.len(), 1);
    assert_eq!(persisted.messages[0].role, "user");
    assert_eq!(persisted.messages[0].content.as_deref(), Some("hello"));

    let repeated = store
        .commit_conversation_idempotency_with_limits(
            completion,
            &conversation,
            idempotency_limits(100, 1024, 10),
        )
        .await
        .unwrap();
    assert_eq!(repeated.revision, 1);
    assert!(repeated.already_completed);
}

#[tokio::test]
async fn pg_conversation_claim_checks_durable_revision_before_insert() {
    let Some(url) = pg_url() else {
        eprintln!(
            "SKIP pg_conversation_claim_checks_durable_revision_before_insert: IRONCREW_TEST_PG_URL unset"
        );
        return;
    };
    let prefix = "idem_rev_";
    reset(&url, prefix).await;
    let store = PostgresStore::new(&url, prefix).await.unwrap();
    let conversation = ConversationRecord {
        id: "chat-revision".into(),
        flow_name: "chat".into(),
        flow_path: Some("flow-a".into()),
        agent_name: "assistant".into(),
        messages: vec![ChatMessage::user("existing")],
        created_at: "2026-07-19T12:00:00Z".into(),
        updated_at: "2026-07-19T12:01:00Z".into(),
        revision: 0,
    };
    assert_eq!(store.save_conversation(&conversation).await.unwrap(), 1);

    let stale = idempotency_claim(
        'e',
        'f',
        CONVERSATION_MESSAGE_OPERATION,
        "chat-revision",
        Some("conversation:flow-a:chat-revision"),
        Some(0),
        "2026-07-19T12:10:00Z",
    );
    assert!(matches!(
        store
            .claim_idempotency_with_limits(stale.clone(), default_idempotency_limits())
            .await
            .unwrap(),
        IdempotencyClaimOutcome::Conflict
    ));
    assert!(matches!(
        store
            .lookup_idempotency(
                &stale.key_hash,
                &stale.request_fingerprint,
                "2026-07-19T12:02:00Z",
            )
            .await
            .unwrap(),
        IdempotencyLookup::Miss
    ));

    let current = idempotency_claim(
        'f',
        'e',
        CONVERSATION_MESSAGE_OPERATION,
        "chat-revision",
        Some("conversation:flow-a:chat-revision"),
        Some(1),
        "2026-07-19T12:10:00Z",
    );
    assert!(matches!(
        store
            .claim_idempotency_with_limits(current, default_idempotency_limits())
            .await
            .unwrap(),
        IdempotencyClaimOutcome::Claimed(_)
    ));

    let missing_stale = idempotency_claim(
        '0',
        '1',
        CONVERSATION_MESSAGE_OPERATION,
        "chat-missing",
        Some("conversation:flow-a:chat-missing"),
        Some(1),
        "2026-07-19T12:10:00Z",
    );
    assert!(matches!(
        store
            .claim_idempotency_with_limits(missing_stale, default_idempotency_limits())
            .await
            .unwrap(),
        IdempotencyClaimOutcome::Conflict
    ));
    let missing_zero = idempotency_claim(
        '1',
        '0',
        CONVERSATION_MESSAGE_OPERATION,
        "chat-missing",
        Some("conversation:flow-a:chat-missing"),
        Some(0),
        "2026-07-19T12:10:00Z",
    );
    assert!(matches!(
        store
            .claim_idempotency_with_limits(missing_zero, default_idempotency_limits())
            .await
            .unwrap(),
        IdempotencyClaimOutcome::Claimed(_)
    ));
}

#[tokio::test]
async fn pg_indeterminate_records_cannot_be_completed_or_committed() {
    let Some(url) = pg_url() else {
        eprintln!(
            "SKIP pg_indeterminate_records_cannot_be_completed_or_committed: IRONCREW_TEST_PG_URL unset"
        );
        return;
    };
    let prefix = "idem_ind_";
    reset(&url, prefix).await;
    let store = PostgresStore::new(&url, prefix).await.unwrap();
    let claim = idempotency_claim(
        '2',
        '3',
        CONVERSATION_MESSAGE_OPERATION,
        "chat-indeterminate",
        Some("conversation:flow-a:chat-indeterminate"),
        Some(0),
        "2026-07-19T12:10:00Z",
    );
    assert!(matches!(
        store
            .claim_idempotency_with_limits(claim.clone(), default_idempotency_limits())
            .await
            .unwrap(),
        IdempotencyClaimOutcome::Claimed(_)
    ));
    assert!(
        store
            .mark_idempotency_indeterminate(
                &claim.key_hash,
                &claim.attempt_id,
                "2026-07-19T12:01:00Z",
                "2026-07-20T12:01:00Z",
            )
            .await
            .unwrap()
    );

    let completion = idempotency_completion(&claim, Some("{\"reply\":\"unsafe\"}"));
    assert!(matches!(
        store
            .complete_idempotency_with_limits(completion.clone(), idempotency_limits(100, 1024, 10))
            .await,
        Err(ironcrew::utils::error::IronCrewError::Conflict(_))
    ));
    let conversation = ConversationRecord {
        id: "chat-indeterminate".into(),
        flow_name: "chat".into(),
        flow_path: Some("flow-a".into()),
        agent_name: "assistant".into(),
        messages: vec![ChatMessage::user("must not persist")],
        created_at: "2026-07-19T12:00:00Z".into(),
        updated_at: "2026-07-19T12:01:00Z".into(),
        revision: 0,
    };
    assert!(matches!(
        store
            .commit_conversation_idempotency_with_limits(
                completion,
                &conversation,
                idempotency_limits(100, 1024, 10)
            )
            .await,
        Err(ironcrew::utils::error::IronCrewError::Conflict(_))
    ));
    assert!(
        store
            .get_conversation(Some("flow-a"), "chat-indeterminate")
            .await
            .unwrap()
            .is_none()
    );
    assert!(matches!(
        store
            .lookup_idempotency(
                &claim.key_hash,
                &claim.request_fingerprint,
                "2026-07-19T12:02:00Z",
            )
            .await
            .unwrap(),
        IdempotencyLookup::Indeterminate(_)
    ));
}

#[tokio::test]
async fn pg_idempotency_retention_prunes_only_terminal_records() {
    let Some(url) = pg_url() else {
        eprintln!(
            "SKIP pg_idempotency_retention_prunes_only_terminal_records: IRONCREW_TEST_PG_URL unset"
        );
        return;
    };
    let prefix = "idem_ret_";
    reset(&url, prefix).await;
    let store = PostgresStore::new(&url, prefix).await.unwrap();
    let expired = idempotency_claim(
        'c',
        'd',
        CONVERSATION_MESSAGE_OPERATION,
        "expired",
        Some("conversation:flow-a:expired"),
        Some(0),
        "2026-07-19T12:10:00Z",
    );
    let live = idempotency_claim(
        'd',
        'e',
        RUN_OPERATION,
        "live-run",
        None,
        None,
        "2026-07-19T12:10:00Z",
    );
    store
        .claim_idempotency_with_limits(expired.clone(), default_idempotency_limits())
        .await
        .unwrap();
    store
        .claim_idempotency_with_limits(live.clone(), default_idempotency_limits())
        .await
        .unwrap();
    let usage = store
        .idempotency_usage(&expired.principal_id, default_idempotency_limits())
        .await
        .unwrap();
    assert_eq!(usage.global_records, 2);
    assert_eq!(usage.global_in_flight, 2);
    assert_eq!(usage.global_response_bytes, 0);
    let mut completion = idempotency_completion(&expired, Some("{}"));
    completion.expires_at = "2026-07-19T12:02:00Z".into();
    store
        .complete_idempotency_with_limits(completion, idempotency_limits(100, 1024, 10))
        .await
        .unwrap();
    let usage = store
        .idempotency_usage(&expired.principal_id, default_idempotency_limits())
        .await
        .unwrap();
    assert_eq!(usage.global_records, 2);
    assert_eq!(usage.global_in_flight, 1);
    assert_eq!(usage.global_response_bytes, 2);
    assert!(matches!(
        store
            .lookup_idempotency(
                &expired.key_hash,
                &expired.request_fingerprint,
                "9999-07-19T12:03:00Z"
            )
            .await
            .unwrap(),
        IdempotencyLookup::Replay(_)
    ));
    expire_idempotency_retention(&url, prefix, &expired.key_hash).await;
    assert!(matches!(
        store
            .lookup_idempotency(
                &expired.key_hash,
                &expired.request_fingerprint,
                "1900-07-19T12:03:00Z"
            )
            .await
            .unwrap(),
        IdempotencyLookup::Miss
    ));
    assert_eq!(
        store
            .prune_idempotency("1900-07-19T12:03:00Z", 1)
            .await
            .unwrap(),
        1
    );
    let usage = store
        .idempotency_usage(&expired.principal_id, default_idempotency_limits())
        .await
        .unwrap();
    assert_eq!(usage.global_records, 1);
    assert_eq!(usage.global_in_flight, 1);
    assert_eq!(usage.global_response_bytes, 0);
    assert!(matches!(
        store
            .lookup_idempotency(
                &live.key_hash,
                &live.request_fingerprint,
                "9999-07-19T12:03:00Z"
            )
            .await
            .unwrap(),
        IdempotencyLookup::InProgress(_)
    ));

    let replacement = idempotency_claim(
        'c',
        'f',
        RUN_OPERATION,
        "replacement-run",
        None,
        None,
        "2026-07-19T12:10:00Z",
    );
    assert!(matches!(
        store
            .claim_idempotency_with_limits(replacement, idempotency_limits(2, usize::MAX, 1))
            .await
            .unwrap(),
        IdempotencyClaimOutcome::Claimed(_)
    ));
}

#[tokio::test]
async fn pg_run_claim_enforces_aggregate_response_budget_before_terminalization() {
    let Some(url) = pg_url() else {
        eprintln!(
            "SKIP pg_run_claim_enforces_aggregate_response_budget_before_terminalization: IRONCREW_TEST_PG_URL unset"
        );
        return;
    };
    let prefix = "idem_budget_";
    reset(&url, prefix).await;
    let store = PostgresStore::new_with_lease_config(
        &url,
        prefix,
        RunLeaseConfig::new("owner-a", Duration::from_secs(60)).unwrap(),
    )
    .await
    .unwrap();
    let first_body = "{\"run_id\":\"budget-run-a\"}";
    let second_body = "{\"run_id\":\"budget-run-b\"}";
    let aggregate_cap = first_body.len();

    let mut first = idempotency_claim(
        '2',
        '3',
        RUN_OPERATION,
        "budget-run-a",
        None,
        None,
        "9999-01-01T00:00:00Z",
    );
    first.response_status = Some(202);
    first.response_body = Some(first_body.into());
    let mut second = idempotency_claim(
        '3',
        '4',
        RUN_OPERATION,
        "budget-run-b",
        None,
        None,
        "1900-01-01T00:00:00Z",
    );
    second.response_status = Some(202);
    second.response_body = Some(second_body.into());
    let budget_limits = idempotency_limits(100, aggregate_cap, 10);

    match store
        .claim_idempotency_with_limits(first.clone(), budget_limits)
        .await
        .unwrap()
    {
        IdempotencyClaimOutcome::Claimed(record) => {
            assert_eq!(record.response_status, Some(202));
            assert_eq!(record.response_body.as_deref(), Some(first_body));
        }
        other => panic!("expected first budgeted claim, got {other:?}"),
    }
    match store
        .claim_idempotency_with_limits(second.clone(), budget_limits)
        .await
        .unwrap()
    {
        IdempotencyClaimOutcome::Claimed(record) => {
            assert_eq!(record.response_status, Some(202));
            assert!(record.response_body.is_none());
        }
        other => panic!("expected tombstoned second claim, got {other:?}"),
    }

    for (run_id, finished_at) in [
        ("budget-run-a", "2026-07-19T12:01:00Z"),
        ("budget-run-b", "2026-07-19T12:02:00Z"),
    ] {
        store
            .save_run_intent(intent(run_id, "flow-a", "2026-07-19T12:00:00Z", vec![]))
            .await
            .unwrap();
        store
            .update_run_completion(
                run_id,
                RunCompletion {
                    status: RunStatus::Success,
                    finished_at: finished_at.into(),
                    duration_ms: 1,
                    task_results: vec![],
                    total_tokens: 0,
                    cached_tokens: 0,
                },
            )
            .await
            .unwrap();
    }

    match store
        .lookup_idempotency(
            &first.key_hash,
            &first.request_fingerprint,
            "9999-12-31T23:59:59Z",
        )
        .await
        .unwrap()
    {
        IdempotencyLookup::Replay(record) => {
            assert_eq!(record.state, IdempotencyState::Completed);
            assert_eq!(record.response_body.as_deref(), Some(first_body));
        }
        other => panic!("expected retained run replay, got {other:?}"),
    }
    match store
        .lookup_idempotency(
            &second.key_hash,
            &second.request_fingerprint,
            "1900-01-01T00:00:00Z",
        )
        .await
        .unwrap()
    {
        IdempotencyLookup::Indeterminate(record) => {
            assert_eq!(record.state, IdempotencyState::Completed);
            assert_eq!(record.response_status, Some(202));
            assert!(record.response_body.is_none());
        }
        other => panic!("expected non-replayable run tombstone, got {other:?}"),
    }
}

#[tokio::test]
async fn pg_run_mappings_progress_and_expired_claim_gets_abandoned_fallback() {
    let Some(url) = pg_url() else {
        eprintln!(
            "SKIP pg_run_mappings_progress_and_expired_claim_gets_abandoned_fallback: IRONCREW_TEST_PG_URL unset"
        );
        return;
    };
    let prefix = "idem_run_";
    reset(&url, prefix).await;
    let store = PostgresStore::new_with_lease_config(
        &url,
        prefix,
        RunLeaseConfig::new("owner-a", Duration::from_secs(60)).unwrap(),
    )
    .await
    .unwrap();

    let mut live = idempotency_claim(
        'e',
        'a',
        RUN_OPERATION,
        "mapped-run",
        None,
        None,
        "2026-07-19T12:10:00Z",
    );
    live.response_status = Some(202);
    live.response_body = Some("{\"run_id\":\"mapped-run\"}".into());
    store
        .claim_idempotency_with_limits(live.clone(), default_idempotency_limits())
        .await
        .unwrap();
    store
        .save_run_intent(intent(
            "mapped-run",
            "flow-a",
            "2026-07-19T12:00:01Z",
            vec![],
        ))
        .await
        .unwrap();
    match store
        .lookup_idempotency(
            &live.key_hash,
            &live.request_fingerprint,
            "2026-07-19T12:00:02Z",
        )
        .await
        .unwrap()
    {
        IdempotencyLookup::Replay(record) => assert_eq!(record.state, IdempotencyState::Running),
        other => panic!("expected running replay, got {other:?}"),
    }
    store
        .update_run_completion(
            "mapped-run",
            RunCompletion {
                status: RunStatus::Success,
                finished_at: "2026-07-19T12:01:00Z".into(),
                duration_ms: 59_000,
                task_results: vec![],
                total_tokens: 0,
                cached_tokens: 0,
            },
        )
        .await
        .unwrap();
    match store
        .lookup_idempotency(
            &live.key_hash,
            &live.request_fingerprint,
            "2026-07-19T12:02:00Z",
        )
        .await
        .unwrap()
    {
        IdempotencyLookup::Replay(record) => assert_eq!(record.state, IdempotencyState::Completed),
        other => panic!("expected completed replay, got {other:?}"),
    }

    let mut orphan = idempotency_claim(
        'f',
        'b',
        RUN_OPERATION,
        "orphan-run",
        None,
        None,
        "2026-07-19T11:59:00Z",
    );
    orphan.response_status = Some(202);
    orphan.response_body = Some("{\"run_id\":\"orphan-run\"}".into());
    store
        .claim_idempotency_with_limits(orphan.clone(), default_idempotency_limits())
        .await
        .unwrap();
    expire_idempotency_lease(&url, prefix, &orphan.key_hash).await;
    assert_eq!(
        store
            .reconcile_abandoned_runs("1900-07-19T12:05:00Z")
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        store.get_run("orphan-run").await.unwrap().status,
        RunStatus::Abandoned
    );
    match store
        .lookup_idempotency(
            &orphan.key_hash,
            &orphan.request_fingerprint,
            "9999-07-19T12:06:00Z",
        )
        .await
        .unwrap()
    {
        IdempotencyLookup::Replay(record) => assert_eq!(record.state, IdempotencyState::Completed),
        other => panic!("expected orphan replay, got {other:?}"),
    }
}

#[tokio::test]
async fn pg_run_intent_hydrates_only_linked_provisional_row() {
    let Some(url) = pg_url() else {
        eprintln!(
            "SKIP pg_run_intent_hydrates_only_linked_provisional_row: IRONCREW_TEST_PG_URL unset"
        );
        return;
    };
    let prefix = "idem_hydrate_";
    reset(&url, prefix).await;
    let store = PostgresStore::new_with_lease_config(
        &url,
        prefix,
        RunLeaseConfig::new("owner-a", Duration::from_secs(60)).unwrap(),
    )
    .await
    .unwrap();
    let mut claim = idempotency_claim(
        'c',
        'd',
        RUN_OPERATION,
        "provisional-run",
        None,
        None,
        "9999-01-01T00:00:00Z",
    );
    claim.response_status = Some(202);
    claim.response_body = Some("{\"run_id\":\"provisional-run\"}".into());
    assert!(matches!(
        store
            .claim_idempotency_with_limits(claim.clone(), default_idempotency_limits())
            .await
            .unwrap(),
        IdempotencyClaimOutcome::Claimed(_)
    ));

    let provisional = RunIntent {
        suggested_id: Some("provisional-run".into()),
        flow_name: "provisional".into(),
        flow: "flow-a".into(),
        started_at: "1900-01-01T00:00:00Z".into(),
        agent_count: 0,
        task_count: 0,
        tags: vec!["provisional".into()],
    };
    assert_eq!(
        store.save_run_intent(provisional).await.unwrap(),
        "provisional-run"
    );
    let hydrated = RunIntent {
        suggested_id: Some("provisional-run".into()),
        flow_name: "hydrated".into(),
        flow: "flow-a".into(),
        started_at: "9999-01-01T00:00:00Z".into(),
        agent_count: 3,
        task_count: 4,
        tags: vec!["hydrated".into()],
    };
    assert_eq!(
        store.save_run_intent(hydrated).await.unwrap(),
        "provisional-run"
    );
    let run = store.get_run("provisional-run").await.unwrap();
    assert_eq!(run.flow_name, "hydrated");
    assert_eq!(run.agent_count, 3);
    assert_eq!(run.task_count, 4);
    assert_eq!(run.tags, vec!["hydrated"]);
    assert_eq!(run.started_at, "1900-01-01T00:00:00Z");

    store
        .complete_idempotency_with_limits(
            idempotency_completion(&claim, Some("{}")),
            idempotency_limits(100, 1024, 10),
        )
        .await
        .unwrap();
    assert_eq!(
        store
            .save_run_intent(RunIntent {
                suggested_id: Some("provisional-run".into()),
                flow_name: "hydrated-completed".into(),
                flow: "flow-a".into(),
                started_at: "9999-01-01T00:00:00Z".into(),
                agent_count: 5,
                task_count: 6,
                tags: vec!["completed-ledger".into()],
            })
            .await
            .unwrap(),
        "provisional-run"
    );
    let run = store.get_run("provisional-run").await.unwrap();
    assert_eq!(run.flow_name, "hydrated-completed");
    assert_eq!(run.agent_count, 5);
    assert_eq!(run.task_count, 6);

    let other_owner = PostgresStore::new_with_lease_config(
        &url,
        prefix,
        RunLeaseConfig::new("owner-b", Duration::from_secs(60)).unwrap(),
    )
    .await
    .unwrap();
    assert!(matches!(
        other_owner
            .save_run_intent(intent(
                "provisional-run",
                "flow-a",
                "2026-07-19T12:00:00Z",
                vec![],
            ))
            .await,
        Err(ironcrew::utils::error::IronCrewError::Conflict(_))
    ));

    store
        .save_run_intent(intent(
            "ordinary-run",
            "flow-a",
            "2026-07-19T12:00:00Z",
            vec!["first".into()],
        ))
        .await
        .unwrap();
    assert!(matches!(
        store
            .save_run_intent(intent(
                "ordinary-run",
                "flow-a",
                "2026-07-19T12:00:01Z",
                vec!["duplicate".into()],
            ))
            .await,
        Err(ironcrew::utils::error::IronCrewError::Conflict(_))
    ));
    assert_eq!(
        store.get_run("ordinary-run").await.unwrap().tags,
        vec!["first"]
    );
}

#[tokio::test]
async fn pg_idempotent_run_heartbeat_renews_both_fences_only() {
    let Some(url) = pg_url() else {
        eprintln!(
            "SKIP pg_idempotent_run_heartbeat_renews_both_fences_only: IRONCREW_TEST_PG_URL unset"
        );
        return;
    };
    let prefix = "idem_fence_";
    reset(&url, prefix).await;
    let store = PostgresStore::new_with_lease_config(
        &url,
        prefix,
        RunLeaseConfig::new("owner-a", Duration::from_secs(60)).unwrap(),
    )
    .await
    .unwrap();
    let mut claim = idempotency_claim(
        'd',
        'e',
        RUN_OPERATION,
        "fenced-run",
        None,
        None,
        "1900-01-01T00:00:01Z",
    );
    claim.response_status = Some(202);
    claim.response_body = Some("{\"run_id\":\"fenced-run\"}".into());
    assert!(matches!(
        store
            .claim_idempotency_with_limits(claim.clone(), default_idempotency_limits())
            .await
            .unwrap(),
        IdempotencyClaimOutcome::Claimed(_)
    ));

    assert_eq!(
        store
            .heartbeat_idempotent_run(
                "fenced-run",
                &claim.key_hash,
                &claim.attempt_id,
                "9999-12-31T23:59:59Z",
            )
            .await
            .unwrap(),
        RunFenceHeartbeat::Owned
    );
    let database_now = postgres_now(&url).await;
    let claimed_record = match store
        .lookup_idempotency(
            &claim.key_hash,
            &claim.request_fingerprint,
            "9999-12-31T23:59:59Z",
        )
        .await
        .unwrap()
    {
        IdempotencyLookup::InProgress(record) => record,
        other => panic!("expected claimed fence after heartbeat, got {other:?}"),
    };
    let claimed_deadline = chrono::DateTime::parse_from_rfc3339(&claimed_record.lease_expires_at)
        .unwrap()
        .with_timezone(&chrono::Utc);
    assert!(claimed_deadline > database_now + chrono::Duration::seconds(55));
    assert!(claimed_deadline <= database_now + chrono::Duration::seconds(60));

    store
        .save_run_intent(intent(
            "fenced-run",
            "flow-a",
            "2026-07-19T12:00:00Z",
            vec![],
        ))
        .await
        .unwrap();
    store
        .save_run_intent(intent(
            "ordinary-heartbeat-run",
            "flow-a",
            "2026-07-19T12:00:00Z",
            vec![],
        ))
        .await
        .unwrap();
    expire_run_lease(&url, prefix, "fenced-run").await;
    expire_idempotency_lease(&url, prefix, &claim.key_hash).await;
    expire_run_lease(&url, prefix, "ordinary-heartbeat-run").await;

    assert_eq!(store.heartbeat_owned_runs().await.unwrap(), 1);
    let linked_run_before = store.get_run("fenced-run").await.unwrap();
    assert!(
        chrono::DateTime::parse_from_rfc3339(&linked_run_before.lease_expires_at)
            .unwrap()
            .with_timezone(&chrono::Utc)
            <= postgres_now(&url).await
    );
    let ordinary_run = store.get_run("ordinary-heartbeat-run").await.unwrap();
    assert!(
        chrono::DateTime::parse_from_rfc3339(&ordinary_run.lease_expires_at)
            .unwrap()
            .with_timezone(&chrono::Utc)
            > postgres_now(&url).await
    );

    assert!(matches!(
        store
            .heartbeat_idempotent_run(
                "fenced-run",
                &claim.key_hash,
                "wrong-attempt",
                "1900-01-01T00:00:00Z",
            )
            .await,
        Err(ironcrew::utils::error::IronCrewError::Conflict(_))
    ));
    let other_owner = PostgresStore::new_with_lease_config(
        &url,
        prefix,
        RunLeaseConfig::new("owner-b", Duration::from_secs(60)).unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(
        other_owner
            .heartbeat_idempotent_run(
                "fenced-run",
                &claim.key_hash,
                &claim.attempt_id,
                "9999-12-31T23:59:59Z",
            )
            .await
            .unwrap(),
        RunFenceHeartbeat::Lost
    );

    assert_eq!(
        store
            .heartbeat_idempotent_run(
                "fenced-run",
                &claim.key_hash,
                &claim.attempt_id,
                "1900-01-01T00:00:00Z",
            )
            .await
            .unwrap(),
        RunFenceHeartbeat::Owned
    );
    let linked_run = store.get_run("fenced-run").await.unwrap();
    let linked_ledger = match store
        .lookup_idempotency(
            &claim.key_hash,
            &claim.request_fingerprint,
            "9999-12-31T23:59:59Z",
        )
        .await
        .unwrap()
    {
        IdempotencyLookup::Replay(record) => record,
        other => panic!("expected running replay after coupled heartbeat, got {other:?}"),
    };
    assert_eq!(linked_ledger.state, IdempotencyState::Running);
    assert_eq!(linked_run.lease_expires_at, linked_ledger.lease_expires_at);

    store
        .update_run_completion(
            "fenced-run",
            RunCompletion {
                status: RunStatus::Success,
                finished_at: "9999-01-01T00:00:00Z".into(),
                duration_ms: 1,
                task_results: vec![],
                total_tokens: 0,
                cached_tokens: 0,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        store
            .heartbeat_idempotent_run(
                "fenced-run",
                &claim.key_hash,
                &claim.attempt_id,
                "1900-01-01T00:00:00Z",
            )
            .await
            .unwrap(),
        RunFenceHeartbeat::Terminal(RunStatus::Success)
    );

    let mut completed_without_run = idempotency_claim(
        'e',
        'f',
        RUN_OPERATION,
        "no-run",
        None,
        None,
        "9999-01-01T00:00:00Z",
    );
    completed_without_run.response_status = Some(202);
    completed_without_run.response_body = Some("{\"run_id\":\"no-run\"}".into());
    store
        .claim_idempotency_with_limits(completed_without_run.clone(), default_idempotency_limits())
        .await
        .unwrap();
    store
        .complete_idempotency_with_limits(
            idempotency_completion(&completed_without_run, Some("{}")),
            idempotency_limits(100, 1024, 10),
        )
        .await
        .unwrap();
    assert_eq!(
        store
            .heartbeat_idempotent_run(
                "no-run",
                &completed_without_run.key_hash,
                &completed_without_run.attempt_id,
                "1900-01-01T00:00:00Z",
            )
            .await
            .unwrap(),
        RunFenceHeartbeat::Lost
    );
    assert_eq!(
        store
            .heartbeat_idempotent_run(
                "missing-run",
                &digest('f'),
                "missing-attempt",
                "9999-01-01T00:00:00Z",
            )
            .await
            .unwrap(),
        RunFenceHeartbeat::Lost
    );
}

#[tokio::test]
async fn pg_keyed_run_cancellation_crosses_instance_boundary() {
    let Some(url) = pg_url() else {
        eprintln!(
            "SKIP pg_keyed_run_cancellation_crosses_instance_boundary: IRONCREW_TEST_PG_URL unset"
        );
        return;
    };
    let prefix = "cancel_run_";
    reset(&url, prefix).await;
    let owner = PostgresStore::new_with_lease_config(
        &url,
        prefix,
        RunLeaseConfig::new("owner-a", Duration::from_secs(60)).unwrap(),
    )
    .await
    .unwrap();
    let peer = PostgresStore::new_with_lease_config(
        &url,
        prefix,
        RunLeaseConfig::new("owner-b", Duration::from_secs(60)).unwrap(),
    )
    .await
    .unwrap();
    let mut claim = idempotency_claim(
        'a',
        'b',
        RUN_OPERATION,
        "cancel-me",
        None,
        None,
        "9999-01-01T00:00:00Z",
    );
    claim.response_status = Some(202);
    claim.response_body = Some("{\"run_id\":\"cancel-me\"}".into());
    assert!(matches!(
        owner
            .claim_idempotency_with_limits(claim.clone(), default_idempotency_limits())
            .await
            .unwrap(),
        IdempotencyClaimOutcome::Claimed(_)
    ));
    owner
        .save_run_intent(intent(
            "cancel-me",
            "flow-a",
            "2026-07-19T12:00:00Z",
            vec![],
        ))
        .await
        .unwrap();

    assert_eq!(
        peer.request_run_cancellation("cancel-me", "other-flow")
            .await
            .unwrap(),
        RunCancellationRequest::NotFound
    );
    assert_eq!(
        peer.request_run_cancellation("cancel-me", "flow-a")
            .await
            .unwrap(),
        RunCancellationRequest::Requested {
            owner_instance_id: "owner-a".into(),
            already_requested: false,
        }
    );
    assert_eq!(
        peer.request_run_cancellation("cancel-me", "flow-a")
            .await
            .unwrap(),
        RunCancellationRequest::Requested {
            owner_instance_id: "owner-a".into(),
            already_requested: true,
        }
    );
    assert_eq!(
        owner
            .heartbeat_idempotent_run(
                "cancel-me",
                &claim.key_hash,
                &claim.attempt_id,
                "9999-12-31T23:59:59Z",
            )
            .await
            .unwrap(),
        RunFenceHeartbeat::CancelRequested
    );

    owner
        .save_run_intent(intent(
            "unkeyed-run",
            "flow-a",
            "2026-07-19T12:00:00Z",
            vec![],
        ))
        .await
        .unwrap();
    assert_eq!(
        peer.request_run_cancellation("unkeyed-run", "flow-a")
            .await
            .unwrap(),
        RunCancellationRequest::NotDurable
    );

    owner
        .update_run_completion(
            "cancel-me",
            RunCompletion {
                status: RunStatus::Aborted,
                finished_at: "2026-07-19T12:01:00Z".into(),
                duration_ms: 1,
                task_results: vec![],
                total_tokens: 0,
                cached_tokens: 0,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        peer.request_run_cancellation("cancel-me", "flow-a")
            .await
            .unwrap(),
        RunCancellationRequest::Terminal(RunStatus::Aborted)
    );
    assert_eq!(
        peer.request_run_cancellation("missing", "flow-a")
            .await
            .unwrap(),
        RunCancellationRequest::NotFound
    );
}
