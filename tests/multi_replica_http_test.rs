//! PostgreSQL-backed HTTP contract tests for two independent IronCrew API
//! replicas. These tests prove shared acceptance/history and durable keyed-run
//! cancellation; they intentionally do not claim Lua execution failover.
#![cfg(feature = "postgres")]

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::time::Duration;

use ironcrew::api::{AppState, create_router};
use ironcrew::engine::postgres_store::PostgresStore;
use ironcrew::engine::run_history::RunStatus;
use ironcrew::engine::store::{RunLeaseConfig, StateStore};

const PREFIX: &str = "multi_rep_http_";
const PARK_FLOW: &str = r#"
local crew = Crew.new({
    goal = "multi-replica cancellation test",
    provider = "openai",
    model = "test",
    api_key = "test",
})
crew:ask_human({ prompt = "Wait for cross-replica cancellation", timeout_s = 600 })
"#;

fn pg_url() -> Option<String> {
    std::env::var("IRONCREW_TEST_PG_URL")
        .ok()
        .filter(|value| !value.is_empty())
}

async fn reset(url: &str) {
    let pool = sqlx::PgPool::connect(url).await.expect("connect for reset");
    for suffix in [
        "runs",
        "conversations",
        "dialogs",
        "audit_events",
        "idempotency",
        "idempotency_accounting",
    ] {
        let sql = format!("DROP TABLE IF EXISTS {PREFIX}{suffix} CASCADE");
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .execute(&pool)
            .await
            .expect("drop prefixed table");
    }
    let function = format!("DROP FUNCTION IF EXISTS {PREFIX}idempotency_acct_fn() CASCADE");
    sqlx::query(sqlx::AssertSqlSafe(function))
        .execute(&pool)
        .await
        .expect("drop accounting function");
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

#[tokio::test]
async fn keyed_run_can_be_replayed_observed_and_cancelled_from_a_peer_replica() {
    let Some(url) = pg_url() else {
        eprintln!(
            "SKIP keyed_run_can_be_replayed_observed_and_cancelled_from_a_peer_replica: \
             IRONCREW_TEST_PG_URL unset"
        );
        return;
    };
    reset(&url).await;

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
        .send()
        .await
        .expect("terminal SSE from peer");
    assert_eq!(terminal_sse.status(), reqwest::StatusCode::OK);
    let terminal_body = terminal_sse.text().await.expect("terminal SSE body");
    assert!(terminal_body.contains("event: run_complete"));
    assert!(terminal_body.contains("\"status\":\"aborted\""));

    owner.task.abort();
    peer.task.abort();
    drop(owner_store);
    drop(peer_store);
    reset(&url).await;
}
