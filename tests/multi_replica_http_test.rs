//! PostgreSQL-backed HTTP contract tests for two independent IronCrew API
//! replicas. These tests prove shared acceptance/history and durable keyed-run
//! cancellation; they intentionally do not claim Lua execution failover.
#![cfg(feature = "postgres")]

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::time::Duration;

use ironcrew::api::{AppState, create_router};
use ironcrew::engine::human_input::HumanInputKeyring;
use ironcrew::engine::postgres_store::PostgresStore;
use ironcrew::engine::run_history::RunStatus;
use ironcrew::engine::store::{RunLeaseConfig, StateStore};

const PREFIX: &str = "multi_rep_http_";
const HITL_PREFIX: &str = "multi_rep_http_hitl_";
const TEST_KEYRING_JSON: &str = r#"{"test-key-v1":"AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE="}"#;
const PARK_FLOW: &str = r#"
local crew = Crew.new({
    goal = "multi-replica cancellation test",
    provider = "openai",
    model = "test",
    api_key = "test",
})
crew:ask_human({ prompt = "Wait for cross-replica cancellation", timeout_s = 600 })
"#;
const HITL_FLOW: &str = r#"
local crew = Crew.new({
    goal = "multi-replica human-input delivery test",
    provider = "openai",
    model = "test",
    api_key = "test",
})
local answer = crew:ask_human({
    prompt = "Approve the cross-replica handoff?",
    choices = { "approved", "rejected" },
    timeout_s = 30,
})
if answer ~= "replica-b-approved" then
    error("unexpected cross-replica human answer")
end
"#;

fn pg_url() -> Option<String> {
    std::env::var("IRONCREW_TEST_PG_URL")
        .ok()
        .filter(|value| !value.is_empty())
}

fn test_keyring() -> HumanInputKeyring {
    HumanInputKeyring::from_json(TEST_KEYRING_JSON, "test-key-v1")
        .expect("deterministic human-input keyring")
}

async fn reset(url: &str, prefix: &str) {
    let pool = sqlx::PgPool::connect(url).await.expect("connect for reset");
    for suffix in [
        "human_inputs",
        "run_events",
        "run_event_state",
        "run_event_usage",
        "runs",
        "conversations",
        "dialogs",
        "audit_events",
        "idempotency",
        "idempotency_accounting",
    ] {
        let sql = format!("DROP TABLE IF EXISTS {prefix}{suffix} CASCADE");
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .execute(&pool)
            .await
            .expect("drop prefixed table");
    }
    let function = format!("DROP FUNCTION IF EXISTS {prefix}idempotency_acct_fn() CASCADE");
    sqlx::query(sqlx::AssertSqlSafe(function))
        .execute(&pool)
        .await
        .expect("drop accounting function");
    let event_function = format!("DROP FUNCTION IF EXISTS {prefix}run_events_acct_fn() CASCADE");
    sqlx::query(sqlx::AssertSqlSafe(event_function))
        .execute(&pool)
        .await
        .expect("drop run-event accounting function");
    pool.close().await;
}

struct Replica {
    base: String,
    task: tokio::task::JoinHandle<()>,
}

async fn spawn_replica(flows_dir: std::path::PathBuf, store: Arc<dyn StateStore>) -> Replica {
    let state = Arc::new(AppState {
        flows_dir,
        auth: Arc::new(ironcrew::api::auth::AuthConfig::disabled()),
        admission: Arc::new(ironcrew::api::admission::AdmissionController::default()),
        accepting_traffic: AtomicBool::new(true),
        active_runs: Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new())),
        active_conversations: Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new())),
        max_active_conversations: 4,
        conversation_permits: Arc::new(tokio::sync::Semaphore::new(4)),
        max_active_runs: 4,
        run_permits: Arc::new(tokio::sync::Semaphore::new(4)),
        max_sse_connections: 8,
        sse_permits: Arc::new(tokio::sync::Semaphore::new(8)),
        max_run_lifetime: Duration::from_secs(60),
        terminal_persistence_failures: AtomicUsize::new(0),
        store_maintenance_healthy: AtomicBool::new(true),
        readiness_cache: tokio::sync::Mutex::new(None),
        idempotency: Default::default(),
        store,
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind replica");
    let address = listener.local_addr().expect("replica address");
    let app = create_router(state);
    let task = tokio::spawn(async move {
        let _ = axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await;
    });
    Replica {
        base: format!("http://{address}"),
        task,
    }
}

async fn wait_for_status(store: &Arc<dyn StateStore>, run_id: &str, expected: RunStatus) {
    for _ in 0..120 {
        if let Ok(record) = store.get_run(run_id).await
            && record.status == expected
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let observed = store.get_run(run_id).await.ok().map(|run| run.status);
    panic!("run {run_id} did not reach {expected}; observed {observed:?}");
}

fn assert_no_store(response: &reqwest::Response) {
    assert_eq!(
        response
            .headers()
            .get(reqwest::header::CACHE_CONTROL)
            .and_then(|value| value.to_str().ok()),
        Some("no-store")
    );
}

async fn wait_for_shared_question(
    client: &reqwest::Client,
    base: &str,
    run_id: &str,
) -> (serde_json::Value, String) {
    for _ in 0..120 {
        let response = client
            .get(format!("{base}/flows/hitl/questions/{run_id}"))
            .send()
            .await
            .expect("list question through peer");
        if response.status() == reqwest::StatusCode::OK {
            assert_no_store(&response);
            let response_text = response.text().await.expect("shared question response");
            let response_body: serde_json::Value =
                serde_json::from_str(&response_text).expect("shared question JSON");
            if let Some(question) = response_body["questions"]
                .as_array()
                .and_then(|questions| questions.first())
            {
                assert_eq!(response_body["status"], "waiting_for_input");
                assert_eq!(response_body["owner_instance_id"], "replica-a");
                assert_eq!(response_body["control_scope"], "shared_store");
                return (question.clone(), response_text);
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("durable human-input question never appeared for run {run_id}");
}

async fn read_sse_until(response: &mut reqwest::Response, marker: &str) -> String {
    tokio::time::timeout(Duration::from_secs(12), async {
        let mut body = String::new();
        loop {
            let chunk = response
                .chunk()
                .await
                .expect("read SSE chunk")
                .expect("SSE closed before expected event");
            body.push_str(&String::from_utf8_lossy(&chunk));
            if body.contains(marker) {
                return body;
            }
        }
    })
    .await
    .expect("timed out waiting for SSE event")
}

fn first_sse_id(body: &str) -> String {
    body.lines()
        .find_map(|line| line.strip_prefix("id:").map(str::trim))
        .filter(|value| !value.is_empty())
        .expect("SSE event id")
        .to_string()
}

#[tokio::test]
async fn keyed_human_input_can_be_listed_and_answered_through_a_peer_replica() {
    let Some(url) = pg_url() else {
        eprintln!(
            "SKIP keyed_human_input_can_be_listed_and_answered_through_a_peer_replica: \
             IRONCREW_TEST_PG_URL unset"
        );
        return;
    };
    reset(&url, HITL_PREFIX).await;

    let temp = tempfile::tempdir().expect("temporary flows root");
    let flows_dir = temp.path().to_path_buf();
    for (flow, source) in [("hitl", HITL_FLOW), ("other", HITL_FLOW)] {
        let flow_dir = flows_dir.join(flow);
        std::fs::create_dir_all(&flow_dir).expect("create flow");
        std::fs::write(flow_dir.join("crew.lua"), source).expect("write flow");
    }

    let keyring = test_keyring();
    let owner_store: Arc<dyn StateStore> = Arc::new(
        PostgresStore::new_with_lease_config_and_human_input_keyring(
            &url,
            HITL_PREFIX,
            RunLeaseConfig::new("replica-a", Duration::from_secs(3)).unwrap(),
            Some(keyring.clone()),
        )
        .await
        .expect("create owner store with human-input keyring"),
    );
    let peer_store: Arc<dyn StateStore> = Arc::new(
        PostgresStore::new_with_lease_config_and_human_input_keyring(
            &url,
            HITL_PREFIX,
            RunLeaseConfig::new("replica-b", Duration::from_secs(3)).unwrap(),
            Some(keyring),
        )
        .await
        .expect("create peer store with human-input keyring"),
    );
    let owner = spawn_replica(flows_dir.clone(), owner_store.clone()).await;
    let peer = spawn_replica(flows_dir, peer_store.clone()).await;
    let client = reqwest::Client::new();

    let started = client
        .post(format!("{}/flows/hitl/run", owner.base))
        .header("Idempotency-Key", "multi-replica-hitl-key-0001")
        .json(&serde_json::json!({}))
        .send()
        .await
        .expect("start keyed human-input run on owner");
    assert_eq!(started.status(), reqwest::StatusCode::OK);
    let started_body: serde_json::Value = started.json().await.expect("started body");
    let run_id = started_body["run_id"].as_str().expect("run id").to_string();
    assert_eq!(started_body["owner_instance_id"], "replica-a");

    let (question, question_response_text) =
        wait_for_shared_question(&client, &peer.base, &run_id).await;
    assert_eq!(question["prompt"], "Approve the cross-replica handoff?");
    assert_eq!(
        question["choices"],
        serde_json::json!(["approved", "rejected"])
    );
    let question_id = question["question_id"]
        .as_str()
        .expect("question id")
        .to_string();
    let secret_answer = "replica-b-approved";
    assert!(!question_response_text.contains(secret_answer));

    // A valid but unrelated flow must not reveal that this run or question
    // exists under `hitl`.
    let wrong_flow = client
        .post(format!("{}/flows/other/answer/{run_id}", peer.base))
        .json(&serde_json::json!({
            "question_id": question_id,
            "answer": secret_answer,
        }))
        .send()
        .await
        .expect("wrong-flow answer");
    assert_eq!(wrong_flow.status(), reqwest::StatusCode::NOT_FOUND);

    let answered = client
        .post(format!("{}/flows/hitl/answer/{run_id}", peer.base))
        .json(&serde_json::json!({
            "question_id": question_id,
            "answer": secret_answer,
        }))
        .send()
        .await
        .expect("answer through peer");
    assert_eq!(answered.status(), reqwest::StatusCode::ACCEPTED);
    assert_no_store(&answered);
    let answer_text = answered.text().await.expect("queued answer body");
    assert!(!answer_text.contains(secret_answer));
    let answer_body: serde_json::Value =
        serde_json::from_str(&answer_text).expect("queued answer JSON");
    assert_eq!(answer_body["status"], "queued");
    assert_eq!(answer_body["owner_instance_id"], "replica-a");
    assert_eq!(answer_body["control_scope"], "shared_store");

    // The mailbox is first-writer-wins and does not reveal whether another
    // caller already supplied an answer.
    let repeated = client
        .post(format!("{}/flows/hitl/answer/{run_id}", peer.base))
        .json(&serde_json::json!({
            "question_id": question_id,
            "answer": "replacement",
        }))
        .send()
        .await
        .expect("repeat answer");
    assert_eq!(repeated.status(), reqwest::StatusCode::NOT_FOUND);

    wait_for_status(&peer_store, &run_id, RunStatus::Success).await;
    let terminal = client
        .get(format!("{}/flows/hitl/runs/{run_id}", peer.base))
        .send()
        .await
        .expect("read terminal run through peer");
    assert_eq!(terminal.status(), reqwest::StatusCode::OK);
    let terminal_text = terminal.text().await.expect("terminal run body");
    assert!(!terminal_text.contains(secret_answer));

    let audit = client
        .get(format!("{}/audit", peer.base))
        .send()
        .await
        .expect("read peer audit log");
    assert_eq!(audit.status(), reqwest::StatusCode::OK);
    let audit_text = audit.text().await.expect("peer audit body");
    assert!(!audit_text.contains(secret_answer));

    owner.task.abort();
    peer.task.abort();
    drop(owner_store);
    drop(peer_store);
    reset(&url, HITL_PREFIX).await;
}

#[tokio::test]
async fn keyed_run_can_be_replayed_observed_and_cancelled_from_a_peer_replica() {
    let Some(url) = pg_url() else {
        eprintln!(
            "SKIP keyed_run_can_be_replayed_observed_and_cancelled_from_a_peer_replica: \
             IRONCREW_TEST_PG_URL unset"
        );
        return;
    };
    reset(&url, PREFIX).await;

    let temp = tempfile::tempdir().expect("temporary flows root");
    let flows_dir = temp.path().to_path_buf();
    let flow_dir = flows_dir.join("park");
    std::fs::create_dir_all(&flow_dir).expect("create flow");
    std::fs::write(flow_dir.join("crew.lua"), PARK_FLOW).expect("write flow");

    let owner_store: Arc<dyn StateStore> = Arc::new(
        PostgresStore::new_with_lease_config(
            &url,
            PREFIX,
            RunLeaseConfig::new("replica-a", Duration::from_secs(3)).unwrap(),
        )
        .await
        .expect("create owner store"),
    );
    let peer_store: Arc<dyn StateStore> = Arc::new(
        PostgresStore::new_with_lease_config(
            &url,
            PREFIX,
            RunLeaseConfig::new("replica-b", Duration::from_secs(3)).unwrap(),
        )
        .await
        .expect("create peer store"),
    );
    let owner = spawn_replica(flows_dir.clone(), owner_store.clone()).await;
    let peer = spawn_replica(flows_dir, peer_store.clone()).await;
    let client = reqwest::Client::new();
    let key = "multi-replica-key-0001";

    let started = client
        .post(format!("{}/flows/park/run", owner.base))
        .header("Idempotency-Key", key)
        .json(&serde_json::json!({}))
        .send()
        .await
        .expect("start run on owner");
    assert_eq!(started.status(), reqwest::StatusCode::OK);
    let started_body: serde_json::Value = started.json().await.expect("started body");
    let run_id = started_body["run_id"].as_str().expect("run id").to_string();
    assert_eq!(started_body["owner_instance_id"], "replica-a");

    // A peer can observe the active run through the shared bounded journal.
    // Durable human-input events deliberately omit prompt/choice metadata;
    // clients recover it from the separately encrypted questions endpoint.
    let mut initial_sse = client
        .get(format!("{}/flows/park/events/{run_id}", peer.base))
        .send()
        .await
        .expect("active SSE through peer");
    assert_eq!(initial_sse.status(), reqwest::StatusCode::OK);
    assert_eq!(
        initial_sse
            .headers()
            .get(reqwest::header::CACHE_CONTROL)
            .and_then(|value| value.to_str().ok()),
        Some("no-store, no-transform")
    );
    assert_eq!(
        initial_sse
            .headers()
            .get("x-accel-buffering")
            .and_then(|value| value.to_str().ok()),
        Some("no")
    );
    let initial_events = read_sse_until(&mut initial_sse, "human_input_requested").await;
    assert!(!initial_events.contains("Wait for cross-replica cancellation"));
    assert!(initial_events.contains("omitted_from_event_journal"));
    let resume_cursor = first_sse_id(&initial_events);
    assert!(resume_cursor.starts_with(&format!("{run_id}:")));
    drop(initial_sse);

    let remote_questions = client
        .get(format!("{}/flows/park/questions/{run_id}", peer.base))
        .send()
        .await
        .expect("query peer questions");
    assert_eq!(remote_questions.status(), reqwest::StatusCode::CONFLICT);
    let remote_error: serde_json::Value = remote_questions.json().await.expect("owner error");
    assert_eq!(remote_error["code"], "run_owned_by_another_instance");
    assert_eq!(remote_error["owner_instance_id"], "replica-a");

    let replayed = client
        .post(format!("{}/flows/park/run", peer.base))
        .header("Idempotency-Key", key)
        .json(&serde_json::json!({}))
        .send()
        .await
        .expect("replay acceptance on peer");
    assert_eq!(replayed.status(), reqwest::StatusCode::OK);
    assert_eq!(
        replayed
            .headers()
            .get("Idempotency-Replayed")
            .and_then(|value| value.to_str().ok()),
        Some("true")
    );
    let replayed_body: serde_json::Value = replayed.json().await.expect("replayed body");
    assert_eq!(replayed_body["run_id"], run_id);
    assert_eq!(replayed_body["owner_instance_id"], "replica-a");

    let wrong_flow = client
        .post(format!("{}/flows/missing/abort/{run_id}", peer.base))
        .send()
        .await
        .expect("wrong-flow abort");
    assert_eq!(wrong_flow.status(), reqwest::StatusCode::NOT_FOUND);

    let cancelled = client
        .post(format!("{}/flows/park/abort/{run_id}", peer.base))
        .send()
        .await
        .expect("cancel from peer");
    assert_eq!(cancelled.status(), reqwest::StatusCode::OK);
    let cancellation_body: serde_json::Value = cancelled.json().await.expect("cancel body");
    assert_eq!(cancellation_body["status"], "cancellation_requested");
    assert_eq!(cancellation_body["owner_instance_id"], "replica-a");
    assert_eq!(cancellation_body["control_scope"], "shared_store");

    wait_for_status(&peer_store, &run_id, RunStatus::Aborted).await;
    let terminal_sse = client
        .get(format!("{}/flows/park/events/{run_id}", peer.base))
        .header("Last-Event-ID", &resume_cursor)
        .send()
        .await
        .expect("terminal SSE from peer");
    assert_eq!(terminal_sse.status(), reqwest::StatusCode::OK);
    let terminal_body = terminal_sse.text().await.expect("terminal SSE body");
    assert!(terminal_body.contains("event: run_complete"));
    assert!(terminal_body.contains("\"status\":\"aborted\""));
    assert!(terminal_body.contains(&format!("id: {run_id}:")));
    assert!(!terminal_body.contains("human_input_requested"));

    let wrong_cursor = client
        .get(format!("{}/flows/park/events/{run_id}", peer.base))
        .header("Last-Event-ID", "different-run:1")
        .send()
        .await
        .expect("cross-run cursor response");
    assert_eq!(wrong_cursor.status(), reqwest::StatusCode::BAD_REQUEST);
    assert_no_store(&wrong_cursor);
    let wrong_cursor_body: serde_json::Value =
        wrong_cursor.json().await.expect("cross-run cursor JSON");
    assert_eq!(wrong_cursor_body["code"], "cursor_cross_run");

    let ahead_cursor = client
        .get(format!("{}/flows/park/events/{run_id}", peer.base))
        .header("Last-Event-ID", format!("{run_id}:999999"))
        .send()
        .await
        .expect("ahead cursor response");
    assert_eq!(ahead_cursor.status(), reqwest::StatusCode::CONFLICT);
    assert_no_store(&ahead_cursor);
    let ahead_cursor_body: serde_json::Value =
        ahead_cursor.json().await.expect("ahead cursor JSON");
    assert_eq!(ahead_cursor_body["code"], "cursor_ahead");

    owner.task.abort();
    peer.task.abort();
    drop(owner_store);
    drop(peer_store);
    reset(&url, PREFIX).await;
}
