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

use ironcrew::api::idempotency::RunLeaseHeartbeat;
use ironcrew::engine::audit::{AuditEvent, AuditFilter};
use ironcrew::engine::human_input::{
    DurableHumanInputRegistration, HumanInputAnswerOutcome, HumanInputKeyring,
    HumanInputListOutcome, HumanInputReadOutcome, HumanInputRegistrationOutcome,
};
use ironcrew::engine::idempotency::{
    CONVERSATION_MESSAGE_OPERATION, IdempotencyClaim, IdempotencyClaimOutcome,
    IdempotencyCompletion, IdempotencyLimits, IdempotencyLookup, IdempotencyQuotaResource,
    IdempotencyQuotaScope, IdempotencyState, PrincipalId, RUN_OPERATION, RunCancellationRequest,
    RunFenceHeartbeat,
};
use ironcrew::engine::input_bridge::QuestionInfo;
use ironcrew::engine::postgres_store::PostgresStore;
use ironcrew::engine::reconciler::{
    heartbeat_owned_runs_bounded, maintain_run_leases, reconcile_stuck_runs_at,
};
use ironcrew::engine::run_events::{
    EventJournalScope, RunEventAppendBatch, RunEventAppendEntry, RunEventGapReason,
    RunEventJournalConfig,
};
use ironcrew::engine::run_history::{
    ListRunsFilter, RunCompletion, RunIntent, RunStatus, RunTransition,
};
use ironcrew::engine::sessions::{ConversationRecord, DialogStateRecord};
use ironcrew::engine::store::{RunLeaseConfig, StateStore, run_maintenance_database_timeout};
use ironcrew::engine::task::TaskResult;
use ironcrew::llm::provider::ChatMessage;
use ironcrew::lua::dialog::DialogTurn;
use ironcrew::utils::error::IronCrewError;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

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
        "run_events",
        "run_event_state",
        "run_event_usage",
        "runs",
        "conversations",
        "dialogs",
        "audit_events",
        "idempotency",
        "idempotency_accounting",
        "human_inputs",
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
    let function = format!("DROP FUNCTION IF EXISTS {prefix}run_events_acct_fn() CASCADE");
    sqlx::query(sqlx::AssertSqlSafe(function))
        .execute(&pool)
        .await
        .expect("drop run-event accounting function");
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

fn human_input_keyring() -> HumanInputKeyring {
    HumanInputKeyring::from_json(
        r#"{"primary":"BwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwc="}"#,
        "primary",
    )
    .unwrap()
}

fn human_input_registration(
    run_id: &str,
    question_id: &str,
    key_hash: &str,
    attempt_id: &str,
) -> DurableHumanInputRegistration {
    DurableHumanInputRegistration {
        flow: "flow-a".into(),
        run_id: run_id.into(),
        question: QuestionInfo {
            question_id: question_id.into(),
            prompt: "Approve the production rollout?".into(),
            choices: vec!["approve".into(), "reject".into()],
            asked_at: "2026-07-19T12:00:00Z".into(),
            timeout_s: 60,
            kind: "approval".into(),
        },
        key_hash: key_hash.into(),
        attempt_id: attempt_id.into(),
    }
}

fn run_event_config() -> RunEventJournalConfig {
    RunEventJournalConfig {
        max_events_per_run: 16,
        max_bytes_per_run: 16 * 1024,
        max_event_bytes: 1024,
        retention: Duration::from_secs(60),
        max_total_events: 64,
        max_total_bytes: 64 * 1024,
        page_max_events: 16,
        page_max_bytes: 16 * 1024,
        poll_interval: Duration::from_millis(100),
        read_timeout: Duration::from_secs(2),
        prune_batch: 64,
    }
}

fn run_event(sequence: u64, event_type: &str, padding_bytes: usize) -> RunEventAppendEntry {
    RunEventAppendEntry::new(
        sequence,
        event_type,
        serde_json::json!({
            "event": event_type,
            "data": {
                "sequence": sequence,
                "padding": "x".repeat(padding_bytes),
            }
        }),
        1024,
    )
    .unwrap()
}

fn terminal_run_event(
    sequence: u64,
    run_id: &str,
    status: RunStatus,
    duration_ms: u64,
    total_tokens: u32,
) -> RunEventAppendEntry {
    RunEventAppendEntry::new(
        sequence,
        "run_complete",
        serde_json::json!({
            "event": "run_complete",
            "data": {
                "run_id": run_id,
                "status": status.to_string(),
                "duration_ms": duration_ms,
                "total_tokens": total_tokens,
            }
        }),
        1024,
    )
    .unwrap()
}

fn run_event_batch(
    run_id: &str,
    owner_instance_id: &str,
    entries: Vec<RunEventAppendEntry>,
) -> RunEventAppendBatch {
    RunEventAppendBatch {
        run_id: run_id.into(),
        flow: "flow-a".into(),
        owner_instance_id: owner_instance_id.into(),
        entries,
    }
}

async fn journal_store(
    url: &str,
    prefix: &str,
    owner_instance_id: &str,
    config: RunEventJournalConfig,
) -> PostgresStore {
    PostgresStore::new_with_runtime_config(
        url,
        prefix,
        RunLeaseConfig::new(owner_instance_id, Duration::from_secs(60)).unwrap(),
        None,
        config,
    )
    .await
    .unwrap()
}

async fn create_keyed_run(store: &PostgresStore, run_id: &str, key: char) -> IdempotencyClaim {
    let mut claim = idempotency_claim(
        key,
        char::from_u32(key as u32 + 1).unwrap(),
        RUN_OPERATION,
        run_id,
        None,
        None,
        "9999-01-01T00:00:00Z",
    );
    claim.response_status = Some(202);
    claim.response_body = Some(format!(r#"{{"run_id":"{run_id}"}}"#));
    assert!(matches!(
        store
            .claim_idempotency_with_limits(claim.clone(), default_idempotency_limits())
            .await
            .unwrap(),
        IdempotencyClaimOutcome::Claimed(_)
    ));
    store
        .save_run_intent(intent(run_id, "flow-a", "2026-07-19T12:00:00Z", vec![]))
        .await
        .unwrap();
    claim
}

fn assert_database_maintenance_timeout(error: &IronCrewError, operation: &str) {
    let message = error.to_string();
    assert!(
        message.contains("timeout"),
        "{operation} must report the PostgreSQL timeout, got: {message}"
    );
    assert!(
        !message.contains("Run lease"),
        "{operation} hit the outer watchdog before PostgreSQL cancelled and returned its connection: {message}"
    );
}

fn configured_postgres_pool_size() -> usize {
    std::env::var("IRONCREW_DB_POOL_SIZE")
        .ok()
        .map(|value| {
            value
                .parse::<usize>()
                .expect("IRONCREW_DB_POOL_SIZE must be numeric when live PostgreSQL tests run")
        })
        .unwrap_or(10)
}

async fn wait_for_blocked_health_probes(pool: &sqlx::PgPool, table_name: &str, expected: usize) {
    let query_pattern = format!("%UPDATE {table_name} SET lease_expires_at%");
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let blocked: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM pg_stat_activity \
                 WHERE datname = current_database() AND wait_event_type = 'Lock' \
                   AND query LIKE $1",
            )
            .bind(&query_pattern)
            .fetch_one(pool)
            .await
            .expect("inspect blocked PostgreSQL health probes");
            if blocked >= i64::try_from(expected).expect("pool size fits i64") {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("all store connections must reach the held table lock");
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
    assert_eq!(idempotency_indexes, 5, "primary key plus four indexes");
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
async fn pg_run_maintenance_advisory_lock_timeout_recovers_pool() {
    let Some(url) = pg_url() else {
        eprintln!(
            "SKIP pg_run_maintenance_advisory_lock_timeout_recovers_pool: IRONCREW_TEST_PG_URL unset"
        );
        return;
    };
    let prefix = "maint_adv_";
    reset(&url, prefix).await;
    let store = Arc::new(
        PostgresStore::new_with_lease_config(
            &url,
            prefix,
            RunLeaseConfig::new("maintenance-owner", Duration::from_secs(12)).unwrap(),
        )
        .await
        .unwrap(),
    );
    store
        .save_run_intent(intent(
            "maintenance-run",
            "demo",
            "2026-08-07T10:00:00Z",
            vec![],
        ))
        .await
        .unwrap();
    let maintenance: Arc<dyn StateStore> = store.clone();
    let watchdog = store.run_maintenance_watchdog().unwrap();
    let maintenance_healthy = AtomicBool::new(true);

    let pool = sqlx::PgPool::connect(&url).await.unwrap();
    let mut blocker = pool.begin().await.unwrap();
    let lock_name = format!("ironcrew:{prefix}idempotency:run-fence:6:global");
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(lock_name)
        .execute(&mut *blocker)
        .await
        .unwrap();

    let started = Instant::now();
    let failed_cycle = maintain_run_leases(&maintenance, &maintenance_healthy).await;
    let heartbeat_error = failed_cycle
        .heartbeat
        .expect_err("the held run-fence advisory lock must time out the heartbeat");
    assert_database_maintenance_timeout(&heartbeat_error, "heartbeat advisory lock");
    let reconcile_error = failed_cycle
        .reconciliation
        .expect_err("the held run-fence advisory lock must also time out reconciliation");
    assert_database_maintenance_timeout(&reconcile_error, "reconciliation advisory lock");
    assert!(
        !maintenance_healthy.load(Ordering::Acquire),
        "a timed-out maintenance cycle must make readiness pessimistic"
    );
    assert!(
        started.elapsed() < watchdog + watchdog + Duration::from_secs(1),
        "the contended maintenance cycle exceeded two bounded operation windows"
    );

    blocker.rollback().await.unwrap();
    let recovered_cycle = maintain_run_leases(&maintenance, &maintenance_healthy).await;
    assert_eq!(
        recovered_cycle.heartbeat.unwrap(),
        1,
        "the released pool must renew its owned run"
    );
    assert_eq!(
        recovered_cycle.reconciliation.unwrap(),
        0,
        "a successful cycle must preserve the healthy owner's run"
    );
    assert!(
        maintenance_healthy.load(Ordering::Acquire),
        "only a complete successful maintenance cycle may restore readiness"
    );
    assert_eq!(
        store.get_run("maintenance-run").await.unwrap().status,
        RunStatus::Running
    );
    pool.close().await;
}

#[tokio::test]
async fn pg_run_maintenance_row_lock_statement_timeout_recovers_pool() {
    let Some(url) = pg_url() else {
        eprintln!(
            "SKIP pg_run_maintenance_row_lock_statement_timeout_recovers_pool: IRONCREW_TEST_PG_URL unset"
        );
        return;
    };
    let prefix = "maint_row_";
    reset(&url, prefix).await;
    let store = Arc::new(
        PostgresStore::new_with_lease_config(
            &url,
            prefix,
            RunLeaseConfig::new("maintenance-owner", Duration::from_secs(12)).unwrap(),
        )
        .await
        .unwrap(),
    );
    store
        .save_run_intent(intent("locked-run", "demo", "2026-08-07T10:00:00Z", vec![]))
        .await
        .unwrap();
    let before = chrono::DateTime::parse_from_rfc3339(
        &store.get_run("locked-run").await.unwrap().lease_expires_at,
    )
    .unwrap();
    let maintenance: Arc<dyn StateStore> = store.clone();

    let pool = sqlx::PgPool::connect(&url).await.unwrap();
    let mut blocker = pool.begin().await.unwrap();
    let lock_sql = format!("SELECT run_id FROM {prefix}runs WHERE run_id = $1 FOR UPDATE");
    sqlx::query(sqlx::AssertSqlSafe(lock_sql))
        .bind("locked-run")
        .execute(&mut *blocker)
        .await
        .unwrap();

    let error = heartbeat_owned_runs_bounded(&maintenance)
        .await
        .expect_err("the held run row must time out the heartbeat statement");
    assert_database_maintenance_timeout(&error, "heartbeat row lock");
    assert!(
        error.to_string().contains("statement timeout"),
        "the row-lock wait must be cancelled by PostgreSQL statement_timeout: {error}"
    );

    blocker.rollback().await.unwrap();
    assert_eq!(heartbeat_owned_runs_bounded(&maintenance).await.unwrap(), 1);
    let after = chrono::DateTime::parse_from_rfc3339(
        &store.get_run("locked-run").await.unwrap().lease_expires_at,
    )
    .unwrap();
    assert!(after > before, "the recovered pool must renew the held run");
    pool.close().await;
}

#[tokio::test]
async fn pg_run_maintenance_pool_acquisition_is_bounded_and_reusable() {
    let Some(url) = pg_url() else {
        eprintln!(
            "SKIP pg_run_maintenance_pool_acquisition_is_bounded_and_reusable: IRONCREW_TEST_PG_URL unset"
        );
        return;
    };
    let prefix = "maint_pool_";
    reset(&url, prefix).await;
    let store = Arc::new(
        PostgresStore::new_with_lease_config(
            &url,
            prefix,
            RunLeaseConfig::new("maintenance-owner", Duration::from_secs(12)).unwrap(),
        )
        .await
        .unwrap(),
    );
    let maintenance: Arc<dyn StateStore> = store.clone();
    let pool_size = configured_postgres_pool_size();
    assert!((1..=128).contains(&pool_size));

    let pool = sqlx::PgPool::connect(&url).await.unwrap();
    let mut blocker = pool.begin().await.unwrap();
    let table_name = format!("{prefix}runs");
    let lock_sql = format!("LOCK TABLE {table_name} IN ACCESS EXCLUSIVE MODE");
    sqlx::query(sqlx::AssertSqlSafe(lock_sql))
        .execute(&mut *blocker)
        .await
        .unwrap();

    let mut probes = Vec::with_capacity(pool_size);
    for _ in 0..pool_size {
        let store = store.clone();
        probes.push(tokio::spawn(async move { store.health_check().await }));
    }
    wait_for_blocked_health_probes(&pool, &table_name, pool_size).await;

    let watchdog = store.run_maintenance_watchdog().unwrap();
    let started = Instant::now();
    let error = heartbeat_owned_runs_bounded(&maintenance)
        .await
        .expect_err("pool acquisition must not outlive the maintenance watchdog");
    assert!(
        error
            .to_string()
            .contains("Run lease heartbeat exceeded its"),
        "pool exhaustion must report the outer maintenance watchdog: {error}"
    );
    assert!(
        started.elapsed() < watchdog + Duration::from_secs(1),
        "pool acquisition exceeded its bounded maintenance window"
    );

    blocker.rollback().await.unwrap();
    for probe in probes {
        tokio::time::timeout(Duration::from_secs(10), probe)
            .await
            .expect("released health probe must finish")
            .expect("health probe task must not panic")
            .expect("health probe must reuse its released connection");
    }
    assert_eq!(heartbeat_owned_runs_bounded(&maintenance).await.unwrap(), 0);
    pool.close().await;
}

#[tokio::test]
async fn pg_run_maintenance_keyed_heartbeat_keeps_database_sampled_deadline() {
    let Some(url) = pg_url() else {
        eprintln!(
            "SKIP pg_run_maintenance_keyed_heartbeat_keeps_database_sampled_deadline: IRONCREW_TEST_PG_URL unset"
        );
        return;
    };
    let prefix = "maint_latency_";
    reset(&url, prefix).await;
    let lease_ttl = Duration::from_secs(12);
    let store = PostgresStore::new_with_lease_config(
        &url,
        prefix,
        RunLeaseConfig::new("owner-a", lease_ttl).unwrap(),
    )
    .await
    .unwrap();
    let claim = create_keyed_run(&store, "latency-run", 'a').await;

    let pool = sqlx::PgPool::connect(&url).await.unwrap();
    let function_name = format!("{prefix}delay_heartbeat_fn");
    let trigger_name = format!("{prefix}delay_heartbeat");
    let table_name = format!("{prefix}runs");
    let function_sql = format!(
        "CREATE OR REPLACE FUNCTION {function_name}() RETURNS trigger \
         LANGUAGE plpgsql AS $function$ \
         BEGIN PERFORM pg_sleep(0.5); RETURN NEW; END \
         $function$"
    );
    sqlx::query(sqlx::AssertSqlSafe(function_sql))
        .execute(&pool)
        .await
        .unwrap();
    let trigger_sql = format!(
        "CREATE TRIGGER {trigger_name} BEFORE UPDATE OF lease_expires_at ON {table_name} \
         FOR EACH ROW WHEN (OLD.run_id = 'latency-run') \
         EXECUTE FUNCTION {function_name}()"
    );
    sqlx::query(sqlx::AssertSqlSafe(trigger_sql))
        .execute(&pool)
        .await
        .unwrap();

    let started = Instant::now();
    let outcome = store
        .heartbeat_idempotent_run(
            "latency-run",
            &claim.key_hash,
            &claim.attempt_id,
            "9999-08-07T10:00:00Z",
        )
        .await
        .unwrap();
    let elapsed = started.elapsed();
    assert!(matches!(outcome, RunFenceHeartbeat::Owned));
    assert!(
        elapsed >= Duration::from_millis(450),
        "the injected post-sampling database latency did not execute: {elapsed:?}"
    );

    let observed_at = postgres_now(&url).await;
    let record = store.get_run("latency-run").await.unwrap();
    let durable_deadline = chrono::DateTime::parse_from_rfc3339(&record.lease_expires_at)
        .unwrap()
        .with_timezone(&chrono::Utc);
    let remaining = durable_deadline.signed_duration_since(observed_at);
    let configured_ttl = chrono::Duration::seconds(lease_ttl.as_secs() as i64);
    assert!(remaining > chrono::Duration::zero());
    assert!(
        remaining < configured_ttl - chrono::Duration::milliseconds(350),
        "the durable lease was reset from response time instead of retaining the database-sampled deadline: {remaining}"
    );

    let ledger_sql =
        format!("SELECT lease_expires_at FROM {prefix}idempotency WHERE key_hash = $1");
    let ledger_deadline: String = sqlx::query_scalar(sqlx::AssertSqlSafe(ledger_sql))
        .bind(&claim.key_hash)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(ledger_deadline, record.lease_expires_at);

    let drop_trigger_sql = format!("DROP TRIGGER {trigger_name} ON {table_name}");
    sqlx::query(sqlx::AssertSqlSafe(drop_trigger_sql))
        .execute(&pool)
        .await
        .unwrap();
    let drop_function_sql = format!("DROP FUNCTION {function_name}()");
    sqlx::query(sqlx::AssertSqlSafe(drop_function_sql))
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;
}

#[tokio::test]
async fn pg_run_maintenance_high_cardinality_reconciliation_makes_bounded_progress() {
    let Some(url) = pg_url() else {
        eprintln!(
            "SKIP pg_run_maintenance_high_cardinality_reconciliation_makes_bounded_progress: IRONCREW_TEST_PG_URL unset"
        );
        return;
    };
    let prefix = "maint_batch_";
    reset(&url, prefix).await;
    let store = PostgresStore::new_with_lease_config(
        &url,
        prefix,
        RunLeaseConfig::new("maintenance-owner", Duration::from_secs(60)).unwrap(),
    )
    .await
    .unwrap();
    let pool = sqlx::PgPool::connect(&url).await.unwrap();
    let per_kind = 65_i64;

    for (kind, lease) in [
        ("expired", "2000-01-01T00:00:00Z"),
        ("live", "9999-01-01T00:00:00Z"),
    ] {
        let insert_runs = format!(
            "INSERT INTO {prefix}runs (\
                 run_id, flow_name, flow, status, started_at, finished_at, duration_ms, \
                 task_results, agent_count, task_count, total_tokens, cached_tokens, tags, \
                 owner_instance_id, lease_expires_at\
             ) \
             SELECT $2 || '-' || fixture.number::text, 'flow-a', 'flow-a', 'running', \
                    '2026-08-07T10:00:00Z', '', 0, '[]'::jsonb, 1, 1, 0, 0, \
                    '[]'::jsonb, 'dead-owner', $3 \
             FROM generate_series(1, $1::bigint) AS fixture(number)"
        );
        sqlx::query(sqlx::AssertSqlSafe(insert_runs))
            .bind(per_kind)
            .bind(kind)
            .bind(lease)
            .execute(&pool)
            .await
            .unwrap();
    }

    let insert_run_ledgers = format!(
        "INSERT INTO {prefix}idempotency (\
             key_hash, principal_id, request_fingerprint, operation, scope, resource_id, \
             attempt_id, owner_instance_id, state, lease_expires_at, created_at, \
             updated_at, ttl_seconds\
         ) \
         SELECT md5('expired-key-' || fixture.number::text) || \
                    md5('expired-key-tail-' || fixture.number::text), \
                $2, \
                md5('expired-fingerprint-' || fixture.number::text) || \
                    md5('expired-fingerprint-tail-' || fixture.number::text), \
                $3, 'flow-a', 'expired-' || fixture.number::text, \
                'expired-attempt-' || fixture.number::text, 'dead-owner', 'running', \
                '2000-01-01T00:00:00Z', '2026-08-07T10:00:00Z', \
                '2026-08-07T10:00:00Z', 86400 \
         FROM generate_series(1, $1::bigint) AS fixture(number)"
    );
    sqlx::query(sqlx::AssertSqlSafe(insert_run_ledgers))
        .bind(per_kind)
        .bind(PrincipalId::legacy().as_str())
        .bind(RUN_OPERATION)
        .execute(&pool)
        .await
        .unwrap();
    let insert_fallback_ledgers = format!(
        "INSERT INTO {prefix}idempotency (\
             key_hash, principal_id, request_fingerprint, operation, scope, resource_id, \
             attempt_id, owner_instance_id, state, lease_expires_at, created_at, \
             updated_at, ttl_seconds\
         ) \
         SELECT md5('fallback-key-' || fixture.number::text) || \
                    md5('fallback-key-tail-' || fixture.number::text), \
                $2, \
                md5('fallback-fingerprint-' || fixture.number::text) || \
                    md5('fallback-fingerprint-tail-' || fixture.number::text), \
                $3, 'flow-a', 'fallback-' || fixture.number::text, \
                'fallback-attempt-' || fixture.number::text, 'dead-owner', 'claimed', \
                '2000-01-01T00:00:00Z', '2026-08-07T10:00:00Z', \
                '2026-08-07T10:00:00Z', 86400 \
         FROM generate_series(1, $1::bigint) AS fixture(number)"
    );
    sqlx::query(sqlx::AssertSqlSafe(insert_fallback_ledgers))
        .bind(per_kind)
        .bind(PrincipalId::legacy().as_str())
        .bind(RUN_OPERATION)
        .execute(&pool)
        .await
        .unwrap();
    let insert_conversation_ledgers = format!(
        "INSERT INTO {prefix}idempotency (\
             key_hash, principal_id, request_fingerprint, operation, scope, resource_id, \
             attempt_id, owner_instance_id, state, lease_expires_at, created_at, \
             updated_at, ttl_seconds\
         ) \
         SELECT md5('conversation-key-' || fixture.number::text) || \
                    md5('conversation-key-tail-' || fixture.number::text), \
                $2, \
                md5('conversation-fingerprint-' || fixture.number::text) || \
                    md5('conversation-fingerprint-tail-' || fixture.number::text), \
                $3, 'flow-a', 'conversation-' || fixture.number::text, \
                'conversation-attempt-' || fixture.number::text, 'dead-owner', 'claimed', \
                '2000-01-01T00:00:00Z', '2026-08-07T10:00:00Z', \
                '2026-08-07T10:00:00Z', 86400 \
         FROM generate_series(1, $1::bigint) AS fixture(number)"
    );
    sqlx::query(sqlx::AssertSqlSafe(insert_conversation_ledgers))
        .bind(per_kind)
        .bind(PrincipalId::legacy().as_str())
        .bind(CONVERSATION_MESSAGE_OPERATION)
        .execute(&pool)
        .await
        .unwrap();

    let insert_journals = format!(
        "INSERT INTO {prefix}run_event_state (run_id, flow, owner_instance_id) \
         SELECT 'expired-' || fixture.number::text, 'flow-a', 'dead-owner' \
         FROM generate_series(1, $1::bigint) AS fixture(number)"
    );
    sqlx::query(sqlx::AssertSqlSafe(insert_journals))
        .bind(per_kind)
        .execute(&pool)
        .await
        .unwrap();

    for (kind, created_at, expires_at) in [
        (
            "expired",
            "clock_timestamp()",
            "clock_timestamp() + interval '1 hour'",
        ),
        (
            "live",
            "clock_timestamp() - interval '2 hours'",
            "clock_timestamp() - interval '1 hour'",
        ),
    ] {
        let insert_mailboxes = format!(
            "INSERT INTO {prefix}human_inputs (\
                 run_id, question_id, flow, owner_instance_id, key_hash, attempt_id, \
                 question_digest, question_key_fingerprint, question_nonce, \
                 question_ciphertext, state, created_at, expires_at\
             ) \
             SELECT $2 || '-' || fixture.number::text, 'question-1', 'flow-a', \
                    'dead-owner', \
                    md5($2 || '-key-' || fixture.number::text) || \
                        md5($2 || '-key-tail-' || fixture.number::text), \
                    $2 || '-attempt-' || fixture.number::text, \
                    md5($2 || '-question-' || fixture.number::text) || \
                        md5($2 || '-question-tail-' || fixture.number::text), \
                    'fixture-key', decode('000000000000000000000000', 'hex'), \
                    decode('01', 'hex'), 'pending', {created_at}, {expires_at} \
             FROM generate_series(1, $1::bigint) AS fixture(number)"
        );
        sqlx::query(sqlx::AssertSqlSafe(insert_mailboxes))
            .bind(per_kind)
            .bind(kind)
            .execute(&pool)
            .await
            .unwrap();
    }

    let first = store
        .reconcile_abandoned_runs("1900-08-07T10:00:00Z")
        .await
        .unwrap();
    assert_eq!(first, 64, "the first run batch must stay at its fixed cap");
    let fallback_after_first: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT COUNT(*) FROM {prefix}runs \
         WHERE run_id LIKE 'fallback-%' AND status = 'abandoned'"
    )))
    .fetch_one(&pool)
    .await
    .unwrap();
    let expired_after_first: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT COUNT(*) FROM {prefix}runs \
         WHERE run_id LIKE 'expired-%' AND status = 'abandoned'"
    )))
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((fallback_after_first, expired_after_first), (32, 32));

    let completed_run_ledgers: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT COUNT(*) FROM {prefix}idempotency \
         WHERE operation = $1 AND state = 'completed'"
    )))
    .bind(RUN_OPERATION)
    .fetch_one(&pool)
    .await
    .unwrap();
    let incomplete_journals: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT COUNT(*) FROM {prefix}run_event_state WHERE NOT journal_complete"
    )))
    .fetch_one(&pool)
    .await
    .unwrap();
    let remaining_expired_run_mailboxes: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT COUNT(*) FROM {prefix}human_inputs WHERE run_id LIKE 'expired-%'"
    )))
    .fetch_one(&pool)
    .await
    .unwrap();
    let indeterminate_conversations: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT COUNT(*) FROM {prefix}idempotency \
         WHERE operation = $1 AND state = 'indeterminate'"
    )))
    .bind(CONVERSATION_MESSAGE_OPERATION)
    .fetch_one(&pool)
    .await
    .unwrap();
    let remaining_expired_live_mailboxes: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT COUNT(*) FROM {prefix}human_inputs WHERE run_id LIKE 'live-%'"
    )))
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(completed_run_ledgers, first as i64);
    assert_eq!(incomplete_journals, expired_after_first);
    assert_eq!(
        remaining_expired_run_mailboxes,
        per_kind - expired_after_first
    );
    assert_eq!(indeterminate_conversations, 64);
    assert_eq!(remaining_expired_live_mailboxes, 1);

    let mut total = first;
    for _ in 0..8 {
        if total == usize::try_from(per_kind * 2).unwrap() {
            break;
        }
        total += store
            .reconcile_abandoned_runs("1900-08-07T10:00:00Z")
            .await
            .unwrap();
    }
    assert_eq!(total, usize::try_from(per_kind * 2).unwrap());
    assert_eq!(
        store
            .reconcile_abandoned_runs("1900-08-07T10:00:00Z")
            .await
            .unwrap(),
        0
    );

    let abandoned: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT COUNT(*) FROM {prefix}runs WHERE status = 'abandoned'"
    )))
    .fetch_one(&pool)
    .await
    .unwrap();
    let live: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT COUNT(*) FROM {prefix}runs \
         WHERE run_id LIKE 'live-%' AND status = 'running'"
    )))
    .fetch_one(&pool)
    .await
    .unwrap();
    let incomplete_journals: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT COUNT(*) FROM {prefix}run_event_state WHERE NOT journal_complete"
    )))
    .fetch_one(&pool)
    .await
    .unwrap();
    let completed_run_ledgers: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT COUNT(*) FROM {prefix}idempotency \
         WHERE operation = $1 AND state = 'completed'"
    )))
    .bind(RUN_OPERATION)
    .fetch_one(&pool)
    .await
    .unwrap();
    let indeterminate_conversations: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT COUNT(*) FROM {prefix}idempotency \
         WHERE operation = $1 AND state = 'indeterminate'"
    )))
    .bind(CONVERSATION_MESSAGE_OPERATION)
    .fetch_one(&pool)
    .await
    .unwrap();
    let mailboxes: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT COUNT(*) FROM {prefix}human_inputs"
    )))
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(abandoned, per_kind * 2);
    assert_eq!(live, per_kind);
    assert_eq!(incomplete_journals, per_kind);
    assert_eq!(completed_run_ledgers, per_kind * 2);
    assert_eq!(indeterminate_conversations, per_kind);
    assert_eq!(mailboxes, 0);
    pool.close().await;
}

#[tokio::test]
async fn pg_run_maintenance_aggregate_delay_rolls_back_and_recovers_pool() {
    let Some(url) = pg_url() else {
        eprintln!(
            "SKIP pg_run_maintenance_aggregate_delay_rolls_back_and_recovers_pool: IRONCREW_TEST_PG_URL unset"
        );
        return;
    };
    let prefix = "maint_agg_";
    reset(&url, prefix).await;
    let lease_ttl = Duration::from_secs(12);
    let per_statement_delay = Duration::from_millis(500);
    assert!(per_statement_delay < run_maintenance_database_timeout(lease_ttl));
    let store = Arc::new(
        PostgresStore::new_with_runtime_config(
            &url,
            prefix,
            RunLeaseConfig::new("owner-a", lease_ttl).unwrap(),
            Some(human_input_keyring()),
            run_event_config(),
        )
        .await
        .unwrap(),
    );
    let claim = create_keyed_run(&store, "aggregate-run", 'c').await;
    store
        .append_run_events(&run_event_batch(
            "aggregate-run",
            "owner-a",
            vec![run_event(1, "run_started", 0)],
        ))
        .await
        .unwrap();
    store
        .register_human_input(&human_input_registration(
            "aggregate-run",
            "aggregate-question",
            &claim.key_hash,
            &claim.attempt_id,
        ))
        .await
        .unwrap();
    expire_run_lease(&url, prefix, "aggregate-run").await;
    expire_idempotency_lease(&url, prefix, &claim.key_hash).await;

    let pool = sqlx::PgPool::connect(&url).await.unwrap();
    let function_name = format!("{prefix}delay_reconcile_fn");
    let function_sql = format!(
        "CREATE OR REPLACE FUNCTION {function_name}() RETURNS trigger \
         LANGUAGE plpgsql AS $function$ \
         BEGIN PERFORM pg_sleep(0.5); RETURN NEW; END \
         $function$"
    );
    sqlx::query(sqlx::AssertSqlSafe(function_sql))
        .execute(&pool)
        .await
        .unwrap();
    for (trigger, table, column, predicate) in [
        (
            format!("{prefix}delay_run"),
            format!("{prefix}runs"),
            "status",
            "OLD.run_id = 'aggregate-run'",
        ),
        (
            format!("{prefix}delay_journal"),
            format!("{prefix}run_event_state"),
            "journal_complete",
            "OLD.run_id = 'aggregate-run'",
        ),
        (
            format!("{prefix}delay_ledger"),
            format!("{prefix}idempotency"),
            "state",
            "OLD.resource_id = 'aggregate-run'",
        ),
    ] {
        let trigger_sql = format!(
            "CREATE TRIGGER {trigger} BEFORE UPDATE OF {column} ON {table} \
             FOR EACH ROW WHEN ({predicate}) EXECUTE FUNCTION {function_name}()"
        );
        sqlx::query(sqlx::AssertSqlSafe(trigger_sql))
            .execute(&pool)
            .await
            .unwrap();
    }

    let maintenance: Arc<dyn StateStore> = store.clone();
    let watchdog = store.run_maintenance_watchdog().unwrap();
    let started = Instant::now();
    let error = reconcile_stuck_runs_at(&maintenance, "1900-08-07T10:00:00Z")
        .await
        .expect_err("aggregate statement latency must hit the outer watchdog");
    assert!(
        error
            .to_string()
            .contains("Run lease reconciliation exceeded its"),
        "aggregate latency must report the outer transaction watchdog: {error}"
    );
    assert!(started.elapsed() >= watchdog);
    assert!(started.elapsed() < watchdog + Duration::from_secs(1));

    tokio::time::timeout(Duration::from_secs(5), async {
        for (trigger, table) in [
            (format!("{prefix}delay_run"), format!("{prefix}runs")),
            (
                format!("{prefix}delay_journal"),
                format!("{prefix}run_event_state"),
            ),
            (
                format!("{prefix}delay_ledger"),
                format!("{prefix}idempotency"),
            ),
        ] {
            let drop_trigger_sql = format!("DROP TRIGGER {trigger} ON {table}");
            sqlx::query(sqlx::AssertSqlSafe(drop_trigger_sql))
                .execute(&pool)
                .await
                .unwrap();
        }
        let drop_function_sql = format!("DROP FUNCTION {function_name}()");
        sqlx::query(sqlx::AssertSqlSafe(drop_function_sql))
            .execute(&pool)
            .await
            .unwrap();
    })
    .await
    .expect("SQLx must finish the cancelled statement and roll back its transaction");

    let run_status: String = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT status FROM {prefix}runs WHERE run_id = 'aggregate-run'"
    )))
    .fetch_one(&pool)
    .await
    .unwrap();
    let journal_complete: bool = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT journal_complete FROM {prefix}run_event_state \
         WHERE run_id = 'aggregate-run'"
    )))
    .fetch_one(&pool)
    .await
    .unwrap();
    let ledger_state: String = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT state FROM {prefix}idempotency WHERE key_hash = $1"
    )))
    .bind(&claim.key_hash)
    .fetch_one(&pool)
    .await
    .unwrap();
    let mailbox_rows: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT COUNT(*) FROM {prefix}human_inputs WHERE run_id = 'aggregate-run'"
    )))
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(run_status, "running");
    assert!(journal_complete);
    assert_eq!(ledger_state, "running");
    assert_eq!(mailbox_rows, 1);

    assert_eq!(
        reconcile_stuck_runs_at(&maintenance, "1900-08-07T10:00:00Z")
            .await
            .unwrap(),
        1,
        "the same store pool must recover after the cancelled transaction"
    );
    store.health_check().await.unwrap();
    let run_status: String = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT status FROM {prefix}runs WHERE run_id = 'aggregate-run'"
    )))
    .fetch_one(&pool)
    .await
    .unwrap();
    let journal_complete: bool = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT journal_complete FROM {prefix}run_event_state \
         WHERE run_id = 'aggregate-run'"
    )))
    .fetch_one(&pool)
    .await
    .unwrap();
    let ledger_state: String = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT state FROM {prefix}idempotency WHERE key_hash = $1"
    )))
    .bind(&claim.key_hash)
    .fetch_one(&pool)
    .await
    .unwrap();
    let mailbox_rows: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT COUNT(*) FROM {prefix}human_inputs WHERE run_id = 'aggregate-run'"
    )))
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(run_status, "abandoned");
    assert!(!journal_complete);
    assert_eq!(ledger_state, "completed");
    assert_eq!(mailbox_rows, 0);
    pool.close().await;
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
    let store = Arc::new(
        PostgresStore::new_with_lease_config(
            &url,
            prefix,
            RunLeaseConfig::new("owner-a", Duration::from_secs(60)).unwrap(),
        )
        .await
        .unwrap(),
    );

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

    let mut expired_before_intent = idempotency_claim(
        'd',
        'c',
        RUN_OPERATION,
        "expired-before-intent",
        None,
        None,
        "2026-07-19T12:10:00Z",
    );
    expired_before_intent.response_status = Some(202);
    expired_before_intent.response_body = Some("{\"run_id\":\"expired-before-intent\"}".into());
    store
        .claim_idempotency_with_limits(expired_before_intent.clone(), default_idempotency_limits())
        .await
        .unwrap();
    let established_fence = RunLeaseHeartbeat::start(
        store.clone(),
        "expired-before-intent".into(),
        expired_before_intent.key_hash.clone(),
        expired_before_intent.attempt_id.clone(),
        tokio::time::Instant::now() + Duration::from_secs(60),
    )
    .await
    .expect("the fresh claimed ledger must establish its pre-execution fence");
    drop(established_fence);
    expire_idempotency_lease(&url, prefix, &expired_before_intent.key_hash).await;
    let stale_start = store
        .save_run_intent(intent(
            "expired-before-intent",
            "flow-a",
            "2026-07-19T12:02:01Z",
            vec![],
        ))
        .await
        .expect_err("an expired keyed claim must not publish a live run intent");
    assert!(matches!(
        stale_start,
        ironcrew::utils::error::IronCrewError::Conflict(_)
    ));
    assert!(
        store.get_run("expired-before-intent").await.is_err(),
        "the rejected intent transaction must roll back its provisional run row"
    );
    assert_eq!(
        store
            .reconcile_abandoned_runs("1900-07-19T12:04:00Z")
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        store.get_run("expired-before-intent").await.unwrap().status,
        RunStatus::Abandoned
    );

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

#[tokio::test]
async fn pg_human_input_mailbox_is_encrypted_and_cross_replica() {
    let Some(url) = pg_url() else {
        eprintln!(
            "SKIP pg_human_input_mailbox_is_encrypted_and_cross_replica: IRONCREW_TEST_PG_URL unset"
        );
        return;
    };
    let prefix = "hitl_mailbox_";
    reset(&url, prefix).await;
    let keyring = human_input_keyring();
    let owner = PostgresStore::new_with_lease_config_and_human_input_keyring(
        &url,
        prefix,
        RunLeaseConfig::new("owner-a", Duration::from_secs(60)).unwrap(),
        Some(keyring.clone()),
    )
    .await
    .unwrap();
    let peer_b = PostgresStore::new_with_lease_config_and_human_input_keyring(
        &url,
        prefix,
        RunLeaseConfig::new("owner-b", Duration::from_secs(60)).unwrap(),
        Some(keyring.clone()),
    )
    .await
    .unwrap();
    let peer_c = PostgresStore::new_with_lease_config_and_human_input_keyring(
        &url,
        prefix,
        RunLeaseConfig::new("owner-c", Duration::from_secs(60)).unwrap(),
        Some(keyring),
    )
    .await
    .unwrap();
    assert!(owner.supports_durable_human_input());
    assert!(peer_b.supports_durable_human_input());

    let claim = create_keyed_run(&owner, "hitl-run", '1').await;
    let registration =
        human_input_registration("hitl-run", "question-1", &claim.key_hash, &claim.attempt_id);
    assert_eq!(
        owner.register_human_input(&registration).await.unwrap(),
        HumanInputRegistrationOutcome::Registered
    );

    let listed = peer_b
        .list_human_inputs("flow-a", "hitl-run")
        .await
        .unwrap();
    let HumanInputListOutcome::Shared {
        owner_instance_id,
        questions,
    } = listed
    else {
        panic!("PostgreSQL mailbox should be shared")
    };
    assert_eq!(owner_instance_id, "owner-a");
    assert_eq!(questions.len(), 1);
    assert_eq!(questions[0].owner_instance_id, "owner-a");
    assert_eq!(questions[0].info, registration.question);

    let pool = sqlx::PgPool::connect(&url).await.unwrap();
    let table = format!("{prefix}human_inputs");
    let columns: Vec<String> = sqlx::query_scalar(
        "SELECT column_name FROM information_schema.columns \
         WHERE table_schema = current_schema() AND table_name = $1",
    )
    .bind(&table)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(columns.len(), 17);
    for forbidden in ["prompt", "choices", "question", "answer", "answer_json"] {
        assert!(
            !columns.iter().any(|column| column == forbidden),
            "mailbox exposes plaintext column '{forbidden}'"
        );
    }
    let index_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM pg_indexes \
         WHERE schemaname = current_schema() AND tablename = $1",
    )
    .bind(&table)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(index_count, 4, "primary key plus three bounded indexes");
    let cascading_fk: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM pg_constraint AS con \
         JOIN pg_class AS tbl ON tbl.oid = con.conrelid \
         JOIN pg_namespace AS ns ON ns.oid = tbl.relnamespace \
         WHERE ns.nspname = current_schema() AND tbl.relname = $1 \
           AND con.contype = 'f' AND con.confdeltype = 'c')",
    )
    .bind(&table)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(cascading_fk);
    let (question_nonce, question_ciphertext): (Vec<u8>, Vec<u8>) =
        sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT question_nonce, question_ciphertext FROM {table} \
             WHERE run_id = 'hitl-run' AND question_id = 'question-1'"
        )))
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(question_nonce.len(), 12);
    assert!(question_ciphertext.len() > registration.question.prompt.len());
    assert!(
        !question_ciphertext
            .windows(registration.question.prompt.len())
            .any(|window| window == registration.question.prompt.as_bytes())
    );

    let answer_b = serde_json::json!({"approved": true, "comment": "ship it"});
    let answer_c = serde_json::json!({"approved": false, "comment": "hold"});
    let (result_b, result_c) = tokio::join!(
        peer_b.answer_human_input("flow-a", "hitl-run", "question-1", &answer_b),
        peer_c.answer_human_input("flow-a", "hitl-run", "question-1", &answer_c),
    );
    let outcomes = [result_b.unwrap(), result_c.unwrap()];
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| matches!(outcome, HumanInputAnswerOutcome::Queued { .. }))
            .count(),
        1
    );
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| matches!(outcome, HumanInputAnswerOutcome::AlreadyAnswered))
            .count(),
        1
    );
    let winning_answer = match owner.read_human_input(&registration).await.unwrap() {
        HumanInputReadOutcome::Answered(answer) => answer,
        other => panic!("expected encrypted answer, got {other:?}"),
    };
    assert!(winning_answer == answer_b || winning_answer == answer_c);
    assert_eq!(
        owner.read_human_input(&registration).await.unwrap(),
        HumanInputReadOutcome::Answered(winning_answer.clone()),
        "owner reads do not consume the answer"
    );

    let expire_answered_question = format!(
        "UPDATE {table} SET \
             created_at = clock_timestamp() - interval '2 seconds', \
             expires_at = clock_timestamp() - interval '1 second' \
         WHERE run_id = 'hitl-run' AND question_id = 'question-1'"
    );
    sqlx::query(sqlx::AssertSqlSafe(expire_answered_question))
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        peer_b
            .answer_human_input("flow-a", "hitl-run", "question-1", &answer_b)
            .await
            .unwrap(),
        HumanInputAnswerOutcome::AlreadyAnswered,
        "an accepted answer survives a duplicate request after question expiry"
    );
    owner
        .reconcile_abandoned_runs("2026-07-19T12:00:00Z")
        .await
        .unwrap();
    assert_eq!(
        owner.read_human_input(&registration).await.unwrap(),
        HumanInputReadOutcome::Answered(winning_answer.clone()),
        "reconciliation preserves an accepted answer until the owner consumes it"
    );

    let (answer_nonce, answer_ciphertext): (Vec<u8>, Vec<u8>) =
        sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT answer_nonce, answer_ciphertext FROM {table} \
             WHERE run_id = 'hitl-run' AND question_id = 'question-1'"
        )))
        .fetch_one(&pool)
        .await
        .unwrap();
    let winning_plaintext = serde_json::to_vec(&winning_answer).unwrap();
    assert_eq!(answer_nonce.len(), 12);
    assert!(answer_ciphertext.len() > winning_plaintext.len());
    assert!(
        !answer_ciphertext
            .windows(winning_plaintext.len())
            .any(|window| window == winning_plaintext)
    );
    pool.close().await;

    assert!(owner.close_human_input(&registration).await.unwrap());
    assert_eq!(
        owner.read_human_input(&registration).await.unwrap(),
        HumanInputReadOutcome::NotFound
    );
    owner.health_check().await.unwrap();
}

#[tokio::test]
async fn pg_human_input_rejects_wrong_flow_fence_lease_and_expiry() {
    let Some(url) = pg_url() else {
        eprintln!(
            "SKIP pg_human_input_rejects_wrong_flow_fence_lease_and_expiry: IRONCREW_TEST_PG_URL unset"
        );
        return;
    };
    let prefix = "hitl_fence_";
    reset(&url, prefix).await;
    let keyring = human_input_keyring();
    let owner = PostgresStore::new_with_lease_config_and_human_input_keyring(
        &url,
        prefix,
        RunLeaseConfig::new("owner-a", Duration::from_secs(60)).unwrap(),
        Some(keyring.clone()),
    )
    .await
    .unwrap();
    let peer = PostgresStore::new_with_lease_config_and_human_input_keyring(
        &url,
        prefix,
        RunLeaseConfig::new("owner-b", Duration::from_secs(60)).unwrap(),
        Some(keyring),
    )
    .await
    .unwrap();

    let fence_claim = create_keyed_run(&owner, "fence-run", '1').await;
    let fence_registration = human_input_registration(
        "fence-run",
        "fence-question",
        &fence_claim.key_hash,
        &fence_claim.attempt_id,
    );
    owner
        .register_human_input(&fence_registration)
        .await
        .unwrap();
    assert_eq!(
        peer.answer_human_input(
            "other-flow",
            "fence-run",
            "fence-question",
            &serde_json::json!(true),
        )
        .await
        .unwrap(),
        HumanInputAnswerOutcome::NotFound
    );
    let mut wrong_fence = fence_registration.clone();
    wrong_fence.attempt_id = "wrong-attempt".into();
    assert_eq!(
        owner.read_human_input(&wrong_fence).await.unwrap(),
        HumanInputReadOutcome::NotFound
    );
    let pool = sqlx::PgPool::connect(&url).await.unwrap();
    let corrupt_fence = format!(
        "UPDATE {prefix}human_inputs SET attempt_id = 'wrong-attempt' \
         WHERE run_id = 'fence-run'"
    );
    sqlx::query(sqlx::AssertSqlSafe(corrupt_fence))
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        peer.answer_human_input(
            "flow-a",
            "fence-run",
            "fence-question",
            &serde_json::json!(true),
        )
        .await
        .unwrap(),
        HumanInputAnswerOutcome::NotFound
    );

    let expiry_claim = create_keyed_run(&owner, "expiry-run", '3').await;
    let expiry_registration = human_input_registration(
        "expiry-run",
        "expiry-question",
        &expiry_claim.key_hash,
        &expiry_claim.attempt_id,
    );
    owner
        .register_human_input(&expiry_registration)
        .await
        .unwrap();
    let expire_question = format!(
        "UPDATE {prefix}human_inputs SET \
             created_at = clock_timestamp() - interval '2 seconds', \
             expires_at = clock_timestamp() - interval '1 second' \
         WHERE run_id = 'expiry-run'"
    );
    sqlx::query(sqlx::AssertSqlSafe(expire_question))
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        peer.answer_human_input(
            "flow-a",
            "expiry-run",
            "expiry-question",
            &serde_json::json!(true),
        )
        .await
        .unwrap(),
        HumanInputAnswerOutcome::NotFound
    );

    let lease_claim = create_keyed_run(&owner, "lease-run", '5').await;
    let lease_registration = human_input_registration(
        "lease-run",
        "lease-question",
        &lease_claim.key_hash,
        &lease_claim.attempt_id,
    );
    owner
        .register_human_input(&lease_registration)
        .await
        .unwrap();
    expire_run_lease(&url, prefix, "lease-run").await;
    assert_eq!(
        peer.answer_human_input(
            "flow-a",
            "lease-run",
            "lease-question",
            &serde_json::json!(true),
        )
        .await
        .unwrap(),
        HumanInputAnswerOutcome::NotFound
    );
    pool.close().await;
}

#[tokio::test]
async fn pg_human_input_rows_follow_run_lifecycle_cleanup() {
    let Some(url) = pg_url() else {
        eprintln!(
            "SKIP pg_human_input_rows_follow_run_lifecycle_cleanup: IRONCREW_TEST_PG_URL unset"
        );
        return;
    };
    let prefix = "hitl_cleanup_";
    reset(&url, prefix).await;
    let keyring = human_input_keyring();
    let owner = PostgresStore::new_with_lease_config_and_human_input_keyring(
        &url,
        prefix,
        RunLeaseConfig::new("owner-a", Duration::from_secs(60)).unwrap(),
        Some(keyring.clone()),
    )
    .await
    .unwrap();
    let peer = PostgresStore::new_with_lease_config_and_human_input_keyring(
        &url,
        prefix,
        RunLeaseConfig::new("owner-b", Duration::from_secs(60)).unwrap(),
        Some(keyring),
    )
    .await
    .unwrap();

    let completion_claim = create_keyed_run(&owner, "completion-run", '1').await;
    owner
        .register_human_input(&human_input_registration(
            "completion-run",
            "completion-question",
            &completion_claim.key_hash,
            &completion_claim.attempt_id,
        ))
        .await
        .unwrap();
    owner
        .update_run_completion(
            "completion-run",
            RunCompletion {
                status: RunStatus::Success,
                finished_at: "2026-07-19T12:01:00Z".into(),
                duration_ms: 1,
                task_results: vec![],
                total_tokens: 0,
                cached_tokens: 0,
            },
        )
        .await
        .unwrap();

    let cancellation_claim = create_keyed_run(&owner, "cancellation-run", '3').await;
    owner
        .register_human_input(&human_input_registration(
            "cancellation-run",
            "cancellation-question",
            &cancellation_claim.key_hash,
            &cancellation_claim.attempt_id,
        ))
        .await
        .unwrap();
    assert!(matches!(
        peer.request_run_cancellation("cancellation-run", "flow-a")
            .await
            .unwrap(),
        RunCancellationRequest::Requested { .. }
    ));

    let deletion_claim = create_keyed_run(&owner, "deletion-run", '5').await;
    owner
        .register_human_input(&human_input_registration(
            "deletion-run",
            "deletion-question",
            &deletion_claim.key_hash,
            &deletion_claim.attempt_id,
        ))
        .await
        .unwrap();
    owner.delete_run("deletion-run").await.unwrap();

    let reconciliation_claim = create_keyed_run(&owner, "reconciliation-run", 'a').await;
    owner
        .register_human_input(&human_input_registration(
            "reconciliation-run",
            "reconciliation-question",
            &reconciliation_claim.key_hash,
            &reconciliation_claim.attempt_id,
        ))
        .await
        .unwrap();
    expire_run_lease(&url, prefix, "reconciliation-run").await;
    expire_idempotency_lease(&url, prefix, &reconciliation_claim.key_hash).await;
    owner
        .reconcile_abandoned_runs("2026-07-19T12:02:00Z")
        .await
        .unwrap();

    let pool = sqlx::PgPool::connect(&url).await.unwrap();
    for run_id in [
        "completion-run",
        "cancellation-run",
        "deletion-run",
        "reconciliation-run",
    ] {
        let remaining: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT COUNT(*) FROM {prefix}human_inputs WHERE run_id = $1"
        )))
        .bind(run_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(remaining, 0, "mailbox row leaked for {run_id}");
    }
    pool.close().await;
}

#[tokio::test]
async fn pg_run_event_journal_is_shared_fenced_idempotent_and_gap_aware() {
    let Some(url) = pg_url() else {
        eprintln!(
            "SKIP pg_run_event_journal_is_shared_fenced_idempotent_and_gap_aware: \
             IRONCREW_TEST_PG_URL unset"
        );
        return;
    };
    let prefix = "event_shared_";
    reset(&url, prefix).await;
    let config = run_event_config();
    let owner = journal_store(&url, prefix, "owner-a", config.clone()).await;
    let peer = journal_store(&url, prefix, "owner-b", config.clone()).await;

    assert_eq!(owner.event_journal_scope(), EventJournalScope::SharedStore);
    assert_eq!(owner.event_journal_config(), config);
    owner
        .save_run_intent(intent(
            "journal-run",
            "flow-a",
            "2026-07-19T12:00:00Z",
            vec![],
        ))
        .await
        .unwrap();

    let batch = run_event_batch(
        "journal-run",
        "owner-a",
        vec![
            run_event(1, "crew_started", 0),
            run_event(3, "journal_gap", 0),
        ],
    );
    let appended = owner.append_run_events(&batch).await.unwrap();
    assert_eq!(appended.appended_events, 2);
    assert_eq!(appended.duplicate_events, 0);
    assert_eq!(appended.bounds.latest_sequence, 3);
    assert!(!appended.bounds.journal_complete);

    let page = peer
        .read_run_events("flow-a", "journal-run", 0)
        .await
        .unwrap();
    assert_eq!(
        page.events
            .iter()
            .map(|event| event.sequence)
            .collect::<Vec<_>>(),
        vec![1]
    );
    assert!(page.gap.is_none());
    let gap_page = peer
        .read_run_events("flow-a", "journal-run", 1)
        .await
        .unwrap();
    assert!(gap_page.events.is_empty());
    let gap = gap_page.gap.unwrap();
    assert_eq!((gap.first_sequence, gap.last_sequence), (2, 2));
    assert_eq!(gap.reason, RunEventGapReason::WriterBackpressure);
    let resumed = peer
        .read_run_events("flow-a", "journal-run", 2)
        .await
        .unwrap();
    assert_eq!(
        resumed
            .events
            .iter()
            .map(|event| event.sequence)
            .collect::<Vec<_>>(),
        vec![3]
    );
    assert!(resumed.gap.is_none());

    let duplicate = owner.append_run_events(&batch).await.unwrap();
    assert_eq!(duplicate.appended_events, 0);
    assert_eq!(duplicate.duplicate_events, 2);
    assert_eq!(duplicate.bounds, appended.bounds);

    let conflicting = run_event_batch(
        "journal-run",
        "owner-a",
        vec![run_event(1, "crew_started", 5)],
    );
    let error = owner.append_run_events(&conflicting).await.unwrap_err();
    assert!(
        error
            .to_string()
            .contains("already contains different data"),
        "unexpected conflict: {error}"
    );

    let wrong_owner = run_event_batch(
        "journal-run",
        "owner-b",
        vec![run_event(4, "crew_complete", 0)],
    );
    let error = peer.append_run_events(&wrong_owner).await.unwrap_err();
    assert!(
        error.to_string().contains("is owned by instance 'owner-a'"),
        "unexpected owner fence error: {error}"
    );
    let mut wrong_flow = run_event_batch(
        "journal-run",
        "owner-a",
        vec![run_event(4, "crew_complete", 0)],
    );
    wrong_flow.flow = "other-flow".into();
    let error = owner.append_run_events(&wrong_flow).await.unwrap_err();
    assert!(
        error.to_string().contains("does not match run"),
        "unexpected flow fence error: {error}"
    );
    let error = peer
        .read_run_events("other-flow", "journal-run", 0)
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("does not match run"),
        "unexpected read flow fence error: {error}"
    );

    let pool = sqlx::PgPool::connect(&url).await.unwrap();
    for (table, expected_columns) in [
        (format!("{prefix}run_events"), 8_i64),
        (format!("{prefix}run_event_state"), 11_i64),
        (format!("{prefix}run_event_usage"), 5_i64),
    ] {
        let columns: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM information_schema.columns \
             WHERE table_schema = current_schema() AND table_name = $1",
        )
        .bind(&table)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(columns, expected_columns, "unexpected schema for {table}");
    }
    let event_table = format!("{prefix}run_events");
    let indexes: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM pg_indexes \
         WHERE schemaname = current_schema() AND tablename = $1",
    )
    .bind(&event_table)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(indexes, 3, "primary key plus two bounded prune indexes");
    let accounting_trigger: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM pg_trigger AS trigger_info \
         JOIN pg_class AS table_info ON table_info.oid = trigger_info.tgrelid \
         JOIN pg_namespace AS namespace ON namespace.oid = table_info.relnamespace \
         WHERE namespace.nspname = current_schema() AND table_info.relname = $1 \
           AND trigger_info.tgname = $2 AND NOT trigger_info.tgisinternal)",
    )
    .bind(&event_table)
    .bind(format!("{event_table}_acct_trg"))
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(accounting_trigger);
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE {prefix}run_events SET \
             created_at = clock_timestamp() - interval '2 seconds', \
             expires_at = clock_timestamp() - interval '1 second' \
         WHERE run_id = 'journal-run' AND sequence = 1"
    )))
    .execute(&pool)
    .await
    .unwrap();
    pool.close().await;

    let prefix_gap = peer
        .read_run_events("flow-a", "journal-run", 0)
        .await
        .unwrap();
    assert!(prefix_gap.events.is_empty());
    let gap = prefix_gap.gap.unwrap();
    assert_eq!((gap.first_sequence, gap.last_sequence), (1, 1));
    assert_eq!(gap.reason, RunEventGapReason::Retention);
    let internal_gap = peer
        .read_run_events("flow-a", "journal-run", 1)
        .await
        .unwrap();
    assert!(internal_gap.events.is_empty());
    let gap = internal_gap.gap.unwrap();
    assert_eq!((gap.first_sequence, gap.last_sequence), (2, 2));
    assert_eq!(gap.reason, RunEventGapReason::WriterBackpressure);
    let resumed = peer
        .read_run_events("flow-a", "journal-run", 2)
        .await
        .unwrap();
    assert_eq!(resumed.events.len(), 1);
    assert_eq!(resumed.events[0].sequence, 3);
    owner.health_check().await.unwrap();
}

#[tokio::test]
async fn pg_run_event_journal_enforces_per_run_and_global_caps() {
    let Some(url) = pg_url() else {
        eprintln!(
            "SKIP pg_run_event_journal_enforces_per_run_and_global_caps: \
             IRONCREW_TEST_PG_URL unset"
        );
        return;
    };
    let prefix = "event_caps_";
    reset(&url, prefix).await;
    let config = RunEventJournalConfig {
        max_events_per_run: 3,
        max_bytes_per_run: 3 * 1024,
        max_event_bytes: 1024,
        retention: Duration::from_secs(60),
        max_total_events: 4,
        max_total_bytes: 4 * 1024,
        page_max_events: 3,
        page_max_bytes: 3 * 1024,
        poll_interval: Duration::from_millis(100),
        read_timeout: Duration::from_secs(2),
        prune_batch: 4,
    };
    let store = journal_store(&url, prefix, "owner-a", config.clone()).await;
    for run_id in ["cap-a", "cap-b", "cap-c"] {
        store
            .save_run_intent(intent(run_id, "flow-a", "2026-07-19T12:00:00Z", vec![]))
            .await
            .unwrap();
    }

    store
        .append_run_events(&run_event_batch(
            "cap-a",
            "owner-a",
            vec![
                run_event(1, "task_started", 0),
                run_event(2, "task_complete", 0),
                run_event(3, "crew_started", 0),
            ],
        ))
        .await
        .unwrap();
    let per_run_eviction = store
        .append_run_events(&run_event_batch(
            "cap-a",
            "owner-a",
            vec![run_event(4, "crew_complete", 0)],
        ))
        .await
        .unwrap();
    assert_eq!(per_run_eviction.evicted_events, 1);
    assert_eq!(per_run_eviction.bounds.retained_events, 3);
    let gap = per_run_eviction.eviction_gap.unwrap();
    assert_eq!((gap.first_sequence, gap.last_sequence), (1, 1));
    assert_eq!(gap.reason, RunEventGapReason::WriterBackpressure);

    store
        .append_run_events(&run_event_batch(
            "cap-b",
            "owner-a",
            vec![
                run_event(1, "task_started", 800),
                run_event(2, "task_complete", 800),
            ],
        ))
        .await
        .unwrap();
    let bounded_append = store
        .append_run_events(&run_event_batch(
            "cap-b",
            "owner-a",
            vec![run_event(3, "crew_complete", 800)],
        ))
        .await
        .unwrap();
    assert!(bounded_append.bounds.retained_bytes <= config.max_bytes_per_run as u64);

    store
        .append_run_events(&run_event_batch(
            "cap-c",
            "owner-a",
            vec![
                run_event(1, "task_started", 800),
                run_event(2, "task_complete", 800),
            ],
        ))
        .await
        .unwrap();

    let pool = sqlx::PgPool::connect(&url).await.unwrap();
    let (usage_events, usage_bytes): (i64, i64) = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT retained_events, retained_bytes FROM {prefix}run_event_usage \
         WHERE singleton = TRUE"
    )))
    .fetch_one(&pool)
    .await
    .unwrap();
    let (actual_events, actual_bytes): (i64, i64) = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT COUNT(*)::BIGINT, COALESCE(SUM(accounted_bytes), 0)::BIGINT \
         FROM {prefix}run_events"
    )))
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((usage_events, usage_bytes), (actual_events, actual_bytes));
    assert!(actual_events <= i64::try_from(config.max_total_events).unwrap());
    assert!(actual_bytes <= i64::try_from(config.max_total_bytes).unwrap());
    let run_bounds: Vec<(i64, i64)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT retained_events, retained_bytes FROM {prefix}run_event_state"
    )))
    .fetch_all(&pool)
    .await
    .unwrap();
    assert!(run_bounds.iter().all(|(events, bytes)| {
        *events <= i64::try_from(config.max_events_per_run).unwrap()
            && *bytes <= i64::try_from(config.max_bytes_per_run).unwrap()
    }));
    pool.close().await;

    let evicted_page = store.read_run_events("flow-a", "cap-a", 0).await.unwrap();
    let gap = evicted_page.gap.expect("global cap must evict cap-a");
    assert_eq!(gap.reason, RunEventGapReason::GlobalCapacity);
    assert_eq!(gap.last_sequence, evicted_page.bounds.dropped_through);
}

#[tokio::test]
async fn pg_run_event_journal_retains_terminal_metadata_after_expiry_and_cascades() {
    let Some(url) = pg_url() else {
        eprintln!(
            "SKIP pg_run_event_journal_retains_terminal_metadata_after_expiry_and_cascades: \
             IRONCREW_TEST_PG_URL unset"
        );
        return;
    };
    let prefix = "event_terminal_";
    reset(&url, prefix).await;
    let config = run_event_config();
    let owner = journal_store(&url, prefix, "owner-a", config).await;
    let peer = journal_store(&url, prefix, "owner-b", run_event_config()).await;
    owner
        .save_run_intent(intent(
            "terminal-run",
            "flow-a",
            "2026-07-19T12:00:00Z",
            vec![],
        ))
        .await
        .unwrap();
    owner
        .update_run_completion(
            "terminal-run",
            RunCompletion {
                status: RunStatus::Success,
                finished_at: "2026-07-19T12:01:00Z".into(),
                duration_ms: 321,
                task_results: vec![],
                total_tokens: 42,
                cached_tokens: 7,
            },
        )
        .await
        .unwrap();
    let terminal_batch = run_event_batch(
        "terminal-run",
        "owner-a",
        vec![terminal_run_event(
            1,
            "terminal-run",
            RunStatus::Success,
            321,
            42,
        )],
    );
    owner.append_run_events(&terminal_batch).await.unwrap();
    let duplicate = owner.append_run_events(&terminal_batch).await.unwrap();
    assert_eq!(duplicate.appended_events, 0);
    assert_eq!(duplicate.duplicate_events, 1);

    let page = peer
        .read_run_events("flow-a", "terminal-run", 0)
        .await
        .unwrap();
    let terminal = page.terminal.unwrap();
    assert_eq!(terminal.status, RunStatus::Success);
    assert_eq!(terminal.duration_ms, 321);
    assert_eq!(terminal.total_tokens, 42);
    assert_eq!(terminal.event_sequence, Some(1));

    let pool = sqlx::PgPool::connect(&url).await.unwrap();
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE {prefix}run_events SET \
             created_at = clock_timestamp() - interval '2 seconds', \
             expires_at = clock_timestamp() - interval '1 second' \
         WHERE run_id = 'terminal-run'"
    )))
    .execute(&pool)
    .await
    .unwrap();
    pool.close().await;

    let expired = peer
        .read_run_events("flow-a", "terminal-run", 0)
        .await
        .unwrap();
    assert!(expired.events.is_empty());
    assert_eq!(expired.bounds.latest_sequence, 1);
    assert_eq!(expired.bounds.dropped_through, 1);
    assert_eq!(expired.bounds.retained_events, 0);
    let gap = expired.gap.unwrap();
    assert_eq!(gap.reason, RunEventGapReason::Retention);
    assert_eq!((gap.first_sequence, gap.last_sequence), (1, 1));
    let terminal = expired.terminal.unwrap();
    assert_eq!(terminal.status, RunStatus::Success);
    assert_eq!(terminal.event_sequence, Some(1));

    owner
        .save_run_intent(intent(
            "terminal-without-event",
            "flow-a",
            "2026-07-19T12:02:00Z",
            vec![],
        ))
        .await
        .unwrap();
    owner
        .update_run_completion(
            "terminal-without-event",
            RunCompletion {
                status: RunStatus::Failed,
                finished_at: "2026-07-19T12:03:00Z".into(),
                duration_ms: 5,
                task_results: vec![],
                total_tokens: 2,
                cached_tokens: 0,
            },
        )
        .await
        .unwrap();
    let missing_terminal = peer
        .read_run_events("flow-a", "terminal-without-event", 0)
        .await
        .unwrap();
    assert!(!missing_terminal.bounds.journal_complete);
    let terminal = missing_terminal.terminal.unwrap();
    assert_eq!(terminal.status, RunStatus::Failed);
    assert_eq!(terminal.event_sequence, None);

    owner.delete_run("terminal-run").await.unwrap();
    let pool = sqlx::PgPool::connect(&url).await.unwrap();
    let event_rows: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT COUNT(*) FROM {prefix}run_events WHERE run_id = 'terminal-run'"
    )))
    .fetch_one(&pool)
    .await
    .unwrap();
    let state_rows: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT COUNT(*) FROM {prefix}run_event_state WHERE run_id = 'terminal-run'"
    )))
    .fetch_one(&pool)
    .await
    .unwrap();
    let usage: (i64, i64) = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT retained_events, retained_bytes FROM {prefix}run_event_usage \
         WHERE singleton = TRUE"
    )))
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((event_rows, state_rows), (0, 0));
    assert_eq!(usage, (0, 0));
    pool.close().await;
}

#[tokio::test]
async fn pg_run_event_journal_rejects_stale_and_post_terminal_writes() {
    let Some(url) = pg_url() else {
        eprintln!(
            "SKIP pg_run_event_journal_rejects_stale_and_post_terminal_writes: \
             IRONCREW_TEST_PG_URL unset"
        );
        return;
    };
    let prefix = "event_fence_";
    reset(&url, prefix).await;
    let store = journal_store(&url, prefix, "owner-a", run_event_config()).await;

    store
        .save_run_intent(intent(
            "premature-terminal",
            "flow-a",
            "2026-07-19T12:00:00Z",
            vec![],
        ))
        .await
        .unwrap();
    let premature = run_event_batch(
        "premature-terminal",
        "owner-a",
        vec![terminal_run_event(
            1,
            "premature-terminal",
            RunStatus::Success,
            1,
            1,
        )],
    );
    let error = store.append_run_events(&premature).await.unwrap_err();
    assert!(error.to_string().contains("before its terminal record"));

    store
        .save_run_intent(intent(
            "stale-run",
            "flow-a",
            "2026-07-19T12:00:00Z",
            vec![],
        ))
        .await
        .unwrap();
    expire_run_lease(&url, prefix, "stale-run").await;
    let stale = run_event_batch(
        "stale-run",
        "owner-a",
        vec![run_event(1, "crew_started", 0)],
    );
    let error = store.append_run_events(&stale).await.unwrap_err();
    assert!(error.to_string().contains("active owner lease"));

    store
        .save_run_intent(intent(
            "sealed-run",
            "flow-a",
            "2026-07-19T12:00:00Z",
            vec![],
        ))
        .await
        .unwrap();
    store
        .append_run_events(&run_event_batch(
            "sealed-run",
            "owner-a",
            vec![run_event(1, "crew_started", 0)],
        ))
        .await
        .unwrap();
    store
        .update_run_completion(
            "sealed-run",
            RunCompletion {
                status: RunStatus::Success,
                finished_at: "2026-07-19T12:01:00Z".into(),
                duration_ms: 12,
                task_results: vec![],
                total_tokens: 34,
                cached_tokens: 0,
            },
        )
        .await
        .unwrap();
    let nonterminal = run_event_batch(
        "sealed-run",
        "owner-a",
        vec![run_event(2, "crew_complete", 0)],
    );
    let error = store.append_run_events(&nonterminal).await.unwrap_err();
    assert!(error.to_string().contains("nonterminal journal event"));
    let mismatched = run_event_batch(
        "sealed-run",
        "owner-a",
        vec![terminal_run_event(
            2,
            "sealed-run",
            RunStatus::Failed,
            12,
            34,
        )],
    );
    let error = store.append_run_events(&mismatched).await.unwrap_err();
    assert!(error.to_string().contains("does not match"));

    let terminal = run_event_batch(
        "sealed-run",
        "owner-a",
        vec![terminal_run_event(
            2,
            "sealed-run",
            RunStatus::Success,
            12,
            34,
        )],
    );
    let appended = store.append_run_events(&terminal).await.unwrap();
    assert_eq!(appended.appended_events, 1);
    let duplicate = store.append_run_events(&terminal).await.unwrap();
    assert_eq!(duplicate.duplicate_events, 1);
    let second_terminal = run_event_batch(
        "sealed-run",
        "owner-a",
        vec![terminal_run_event(
            3,
            "sealed-run",
            RunStatus::Success,
            12,
            34,
        )],
    );
    let error = store.append_run_events(&second_terminal).await.unwrap_err();
    assert!(error.to_string().contains("sealed after run_complete"));
    let post_terminal = run_event_batch(
        "sealed-run",
        "owner-a",
        vec![run_event(3, "crew_complete", 0)],
    );
    let error = store.append_run_events(&post_terminal).await.unwrap_err();
    assert!(error.to_string().contains("sealed after run_complete"));

    store
        .save_run_intent(intent(
            "abandoned-run",
            "flow-a",
            "2026-07-19T12:00:00Z",
            vec![],
        ))
        .await
        .unwrap();
    expire_run_lease(&url, prefix, "abandoned-run").await;
    store
        .reconcile_abandoned_runs("2026-07-19T12:05:00Z")
        .await
        .unwrap();
    let abandoned = run_event_batch(
        "abandoned-run",
        "owner-a",
        vec![terminal_run_event(
            1,
            "abandoned-run",
            RunStatus::Abandoned,
            0,
            0,
        )],
    );
    let error = store.append_run_events(&abandoned).await.unwrap_err();
    assert!(error.to_string().contains("Abandoned run"));
}

#[tokio::test]
async fn pg_run_event_journal_reclaims_across_batches_and_survives_restart() {
    let Some(url) = pg_url() else {
        eprintln!(
            "SKIP pg_run_event_journal_reclaims_across_batches_and_survives_restart: \
             IRONCREW_TEST_PG_URL unset"
        );
        return;
    };
    let prefix = "event_multibatch_";
    reset(&url, prefix).await;
    let config = RunEventJournalConfig {
        max_events_per_run: 5,
        max_bytes_per_run: 5 * 1024,
        max_event_bytes: 1024,
        retention: Duration::from_secs(60),
        max_total_events: 20,
        max_total_bytes: 20 * 1024,
        page_max_events: 5,
        page_max_bytes: 5 * 1024,
        poll_interval: Duration::from_millis(100),
        read_timeout: Duration::from_secs(2),
        prune_batch: 2,
    };
    let store = journal_store(&url, prefix, "owner-a", config.clone()).await;
    store
        .save_run_intent(intent(
            "multi-run",
            "flow-a",
            "2026-07-19T12:00:00Z",
            vec![],
        ))
        .await
        .unwrap();
    store
        .append_run_events(&run_event_batch(
            "multi-run",
            "owner-a",
            (1..=5)
                .map(|sequence| run_event(sequence, "task_started", 0))
                .collect(),
        ))
        .await
        .unwrap();
    let reclaimed = store
        .append_run_events(&run_event_batch(
            "multi-run",
            "owner-a",
            (6..=8)
                .map(|sequence| run_event(sequence, "task_complete", 0))
                .collect(),
        ))
        .await
        .unwrap();
    assert_eq!(reclaimed.evicted_events, 3);
    assert_eq!(reclaimed.bounds.dropped_through, 3);
    assert_eq!(reclaimed.bounds.retained_events, 5);

    let pool = sqlx::PgPool::connect(&url).await.unwrap();
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE {prefix}run_event_usage SET schema_version = 0 WHERE singleton = TRUE"
    )))
    .execute(&pool)
    .await
    .unwrap();
    pool.close().await;
    let restarted = journal_store(&url, prefix, "owner-a", config).await;
    let prefix_page = restarted
        .read_run_events("flow-a", "multi-run", 0)
        .await
        .unwrap();
    assert!(prefix_page.events.is_empty());
    let gap = prefix_page.gap.unwrap();
    assert_eq!((gap.first_sequence, gap.last_sequence), (1, 3));
    assert_eq!(gap.reason, RunEventGapReason::WriterBackpressure);
    let retained = restarted
        .read_run_events("flow-a", "multi-run", 3)
        .await
        .unwrap();
    assert_eq!(
        retained
            .events
            .iter()
            .map(|event| event.sequence)
            .collect::<Vec<_>>(),
        vec![4, 5, 6, 7, 8]
    );

    let pool = sqlx::PgPool::connect(&url).await.unwrap();
    let schema_version: i32 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT schema_version FROM {prefix}run_event_usage WHERE singleton = TRUE"
    )))
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(schema_version, 1);
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE {prefix}run_events SET \
             created_at = clock_timestamp() - interval '2 seconds', \
             expires_at = clock_timestamp() - interval '1 second' \
         WHERE run_id = 'multi-run'"
    )))
    .execute(&pool)
    .await
    .unwrap();
    pool.close().await;

    restarted
        .reconcile_abandoned_runs("2026-07-19T12:10:00Z")
        .await
        .unwrap();
    let logically_expired = restarted
        .read_run_events("flow-a", "multi-run", 0)
        .await
        .unwrap();
    assert!(logically_expired.events.is_empty());
    assert_eq!(logically_expired.bounds.dropped_through, 8);
    assert_eq!(logically_expired.bounds.retained_events, 0);
    let gap = logically_expired.gap.unwrap();
    assert_eq!((gap.first_sequence, gap.last_sequence), (1, 8));
    assert_eq!(gap.reason, RunEventGapReason::Retention);

    let pool = sqlx::PgPool::connect(&url).await.unwrap();
    let remaining_after_one_sweep: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT COUNT(*) FROM {prefix}run_events WHERE run_id = 'multi-run'"
    )))
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(remaining_after_one_sweep, 3);
    pool.close().await;
    for _ in 0..2 {
        restarted
            .reconcile_abandoned_runs("2026-07-19T12:10:00Z")
            .await
            .unwrap();
    }
    let pool = sqlx::PgPool::connect(&url).await.unwrap();
    let remaining: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT COUNT(*) FROM {prefix}run_events WHERE run_id = 'multi-run'"
    )))
    .fetch_one(&pool)
    .await
    .unwrap();
    let usage: (i64, i64) = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT retained_events, retained_bytes FROM {prefix}run_event_usage \
         WHERE singleton = TRUE"
    )))
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(remaining, 0);
    assert_eq!(usage, (0, 0));
    pool.close().await;
}

#[tokio::test]
async fn pg_run_event_page_delivers_first_wire_bounded_event_when_db_accounting_is_larger() {
    let Some(url) = pg_url() else {
        eprintln!(
            "SKIP pg_run_event_page_delivers_first_wire_bounded_event_when_db_accounting_is_larger: \
             IRONCREW_TEST_PG_URL unset"
        );
        return;
    };
    let prefix = "event_page_accounting_";
    reset(&url, prefix).await;
    let config = RunEventJournalConfig {
        max_events_per_run: 4,
        max_bytes_per_run: 8 * 1024,
        max_event_bytes: 1024,
        retention: Duration::from_secs(60),
        max_total_events: 16,
        max_total_bytes: 32 * 1024,
        page_max_events: 4,
        page_max_bytes: 1024,
        poll_interval: Duration::from_millis(100),
        read_timeout: Duration::from_secs(2),
        prune_batch: 4,
    };
    let store = journal_store(&url, prefix, "owner-a", config).await;
    store
        .save_run_intent(intent("page-run", "flow-a", "2026-07-19T12:00:00Z", vec![]))
        .await
        .unwrap();
    let mut fields = serde_json::Map::new();
    for index in 0..90 {
        fields.insert(format!("k{index:03}"), serde_json::json!(index));
    }
    let entry = RunEventAppendEntry::new(
        1,
        "log",
        serde_json::json!({"event": "log", "data": fields}),
        1024,
    )
    .unwrap();
    assert!(entry.payload_bytes <= 1024);
    store
        .append_run_events(&run_event_batch("page-run", "owner-a", vec![entry]))
        .await
        .unwrap();
    let pool = sqlx::PgPool::connect(&url).await.unwrap();
    let accounted_bytes: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT accounted_bytes FROM {prefix}run_events \
         WHERE run_id = 'page-run' AND sequence = 1"
    )))
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(accounted_bytes > 1024);
    pool.close().await;

    let page = store
        .read_run_events("flow-a", "page-run", 0)
        .await
        .unwrap();
    assert_eq!(page.events.len(), 1);
    assert_eq!(page.events[0].sequence, 1);
    assert!(page.gap.is_none());
}

#[tokio::test]
async fn pg_reconciliation_commits_when_journal_maintenance_is_unavailable() {
    let Some(url) = pg_url() else {
        eprintln!(
            "SKIP pg_reconciliation_commits_when_journal_maintenance_is_unavailable: \
             IRONCREW_TEST_PG_URL unset"
        );
        return;
    };
    let prefix = "event_reconcile_isolation_";
    reset(&url, prefix).await;
    let store = PostgresStore::new_with_runtime_config(
        &url,
        prefix,
        RunLeaseConfig::new("owner-a", Duration::from_secs(60)).unwrap(),
        Some(human_input_keyring()),
        run_event_config(),
    )
    .await
    .unwrap();
    let claim = create_keyed_run(&store, "isolated-run", '7').await;
    store
        .register_human_input(&human_input_registration(
            "isolated-run",
            "isolated-question",
            &claim.key_hash,
            &claim.attempt_id,
        ))
        .await
        .unwrap();
    expire_run_lease(&url, prefix, "isolated-run").await;
    expire_idempotency_lease(&url, prefix, &claim.key_hash).await;
    let pool = sqlx::PgPool::connect(&url).await.unwrap();
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "DELETE FROM {prefix}run_event_usage WHERE singleton = TRUE"
    )))
    .execute(&pool)
    .await
    .unwrap();
    pool.close().await;

    assert_eq!(
        store
            .reconcile_abandoned_runs("2026-07-19T12:05:00Z")
            .await
            .unwrap(),
        1
    );
    let pool = sqlx::PgPool::connect(&url).await.unwrap();
    let run_status: String = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT status FROM {prefix}runs WHERE run_id = 'isolated-run'"
    )))
    .fetch_one(&pool)
    .await
    .unwrap();
    let ledger_state: String = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT state FROM {prefix}idempotency WHERE key_hash = $1"
    )))
    .bind(&claim.key_hash)
    .fetch_one(&pool)
    .await
    .unwrap();
    let human_rows: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT COUNT(*) FROM {prefix}human_inputs WHERE run_id = 'isolated-run'"
    )))
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(run_status, "abandoned");
    assert_eq!(ledger_state, "completed");
    assert_eq!(human_rows, 0);
    pool.close().await;
}

#[tokio::test]
async fn pg_human_input_registration_enforces_aggregate_capacity() {
    let Some(url) = pg_url() else {
        eprintln!(
            "SKIP pg_human_input_registration_enforces_aggregate_capacity: \
             IRONCREW_TEST_PG_URL unset"
        );
        return;
    };
    if std::env::var_os("IRONCREW_ASK_HUMAN_MAX_PENDING").is_some()
        || std::env::var_os("IRONCREW_ASK_HUMAN_MAX_PENDING_BYTES").is_some()
    {
        eprintln!(
            "SKIP pg_human_input_registration_enforces_aggregate_capacity: \
             explicit mailbox limits override deterministic defaults"
        );
        return;
    }
    let prefix = "hitl_capacity_";
    reset(&url, prefix).await;
    let store = PostgresStore::new_with_runtime_config(
        &url,
        prefix,
        RunLeaseConfig::new("owner-a", Duration::from_secs(60)).unwrap(),
        Some(human_input_keyring()),
        run_event_config(),
    )
    .await
    .unwrap();
    let claim = create_keyed_run(&store, "capacity-run", '8').await;
    let mut last_registration = None;
    for index in 0..16 {
        let registration = human_input_registration(
            "capacity-run",
            &format!("question-{index}"),
            &claim.key_hash,
            &claim.attempt_id,
        );
        assert_eq!(
            store.register_human_input(&registration).await.unwrap(),
            HumanInputRegistrationOutcome::Registered
        );
        last_registration = Some(registration);
    }
    assert_eq!(
        store
            .register_human_input(last_registration.as_ref().unwrap())
            .await
            .unwrap(),
        HumanInputRegistrationOutcome::Registered,
        "an exact retry at capacity must remain idempotent"
    );
    let overflow = human_input_registration(
        "capacity-run",
        "question-overflow",
        &claim.key_hash,
        &claim.attempt_id,
    );
    let error = store.register_human_input(&overflow).await.unwrap_err();
    assert!(error.to_string().contains("configured capacity"));
    let pool = sqlx::PgPool::connect(&url).await.unwrap();
    let retained: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT COUNT(*) FROM {prefix}human_inputs WHERE run_id = 'capacity-run'"
    )))
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(retained, 16);
    pool.close().await;
}
