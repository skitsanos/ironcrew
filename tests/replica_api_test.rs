//! HTTP contract tests for truthful behavior when a request reaches a process
//! that does not own the live run. SQLite supplies a shared transactional test
//! store here; PostgreSQL-specific mailbox semantics have store-level tests.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use ironcrew::api::{AppState, create_router};
use ironcrew::engine::run_history::{RunIntent, RunStatus};
use ironcrew::engine::sqlite_store::SqliteStore;
use ironcrew::engine::store::{RunLeaseConfig, StateStore};

const PARK_FLOW: &str = r#"
local crew = Crew.new({
    goal = "replica API test",
    provider = "openai",
    model = "test",
    api_key = "test",
})
local answer = crew:ask_human({
    prompt = "Continue the replica test?",
    choices = { "yes", "no" },
    timeout_s = 30,
})
if answer ~= "yes" then
    error("unexpected answer")
end
"#;

struct TestReplica {
    base: String,
    state: Arc<AppState>,
    store: Arc<dyn StateStore>,
    server: tokio::task::JoinHandle<()>,
}

impl Drop for TestReplica {
    fn drop(&mut self) {
        self.server.abort();
    }
}

fn app_state(root: &std::path::Path, store: Arc<dyn StateStore>) -> Arc<AppState> {
    let admission = ironcrew::api::admission::AdmissionController::new(
        ironcrew::api::admission::AdmissionConfig {
            work: ironcrew::api::admission::RatePolicy {
                rate_per_minute: 60_000,
                burst: 1_000,
            },
            control: ironcrew::api::admission::RatePolicy {
                rate_per_minute: 60_000,
                burst: 1_000,
            },
        },
    );
    Arc::new(AppState {
        flows_dir: root.to_path_buf(),
        runtime_identity: ironcrew::api::deployment::RuntimeIdentity::disabled(),
        auth: Arc::new(ironcrew::api::auth::AuthConfig::disabled()),
        admission: Arc::new(admission),
        lifecycle: ironcrew::api::lifecycle::LifecycleController::new(),
        active_runs: Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new())),
        active_conversations: Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new())),
        conversation_lifecycles: Arc::new(
            ironcrew::api::conversation_lifecycle::ConversationLifecycleRegistry::new(
                ironcrew::api::conversation_lifecycle::max_active_conversation_lifecycles(),
            ),
        ),
        max_active_conversations: 4,
        conversation_permits: Arc::new(tokio::sync::Semaphore::new(4)),
        max_active_runs: 4,
        run_permits: Arc::new(tokio::sync::Semaphore::new(4)),
        max_active_inspections: 4,
        inspection_permits: Arc::new(tokio::sync::Semaphore::new(4)),
        max_sse_connections: 8,
        sse_permits: Arc::new(tokio::sync::Semaphore::new(8)),
        max_run_lifetime: Duration::from_secs(60),
        terminal_persistence_failures: std::sync::atomic::AtomicUsize::new(0),
        store_maintenance_healthy: std::sync::atomic::AtomicBool::new(true),
        readiness_cache: tokio::sync::Mutex::new(None),
        idempotency: Default::default(),
        store,
    })
}

async fn spawn_replica(root: &std::path::Path, store: Arc<dyn StateStore>) -> TestReplica {
    let state = app_state(root, store.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let app = create_router(state.clone());
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap();
    });
    TestReplica {
        base: format!("http://{address}"),
        state,
        store,
        server,
    }
}

async fn wait_for_question(
    client: &reqwest::Client,
    replica: &TestReplica,
    run_id: &str,
) -> serde_json::Value {
    for _ in 0..200 {
        let response = client
            .get(format!("{}/flows/flow-a/questions/{run_id}", replica.base))
            .send()
            .await
            .unwrap();
        if response.status() == reqwest::StatusCode::OK {
            let body: serde_json::Value = response.json().await.unwrap();
            if let Some(question) = body["questions"].as_array().and_then(|items| items.first()) {
                return question.clone();
            }
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("question did not appear for run {run_id}");
}

async fn wait_for_terminal(store: &Arc<dyn StateStore>, run_id: &str) -> RunStatus {
    for _ in 0..400 {
        if let Ok(record) = store.get_run(run_id).await
            && record.status.is_terminal()
        {
            return record.status;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("run {run_id} did not become terminal");
}

async fn assert_foreign_owner(response: reqwest::Response) {
    assert_eq!(response.status(), reqwest::StatusCode::CONFLICT);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["code"], "run_owned_by_another_instance");
    assert_eq!(body["owner_instance_id"], "owner-a");
    assert_eq!(body["control_scope"], "process");
    assert_eq!(body["retryable"], true);
}

#[tokio::test]
async fn non_owner_replica_reports_ownership_and_recovers_terminal_sse() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    for flow in ["flow-a", "flow-b"] {
        let directory = root.join(flow);
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join("crew.lua"), PARK_FLOW).unwrap();
    }

    let database = root.join("shared.sqlite");
    let store_a: Arc<dyn StateStore> = Arc::new(
        SqliteStore::new_with_lease_config(
            database.clone(),
            RunLeaseConfig::new("owner-a", Duration::from_secs(30)).unwrap(),
        )
        .unwrap(),
    );
    let store_b: Arc<dyn StateStore> = Arc::new(
        SqliteStore::new_with_lease_config(
            database,
            RunLeaseConfig::new("owner-b", Duration::from_secs(30)).unwrap(),
        )
        .unwrap(),
    );
    let replica_a = spawn_replica(root, store_a).await;
    let replica_b = spawn_replica(root, store_b).await;
    let client = reqwest::Client::new();

    let capabilities: serde_json::Value = client
        .get(format!("{}/capabilities", replica_b.base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(capabilities["instance_id"], "owner-b");
    assert_eq!(capabilities["control_scope"], "process");
    assert_eq!(capabilities["multi_replica_control"], false);

    let run_url_a = format!("{}/flows/flow-a/run", replica_a.base);
    let first = client
        .post(&run_url_a)
        .header("Idempotency-Key", "replica-contract-run")
        .json(&serde_json::json!({"test": true}))
        .send()
        .await
        .unwrap();
    assert_eq!(first.status(), reqwest::StatusCode::OK);
    let first_body: serde_json::Value = first.json().await.unwrap();
    let run_id = first_body["run_id"].as_str().unwrap().to_string();
    assert_eq!(first_body["owner_instance_id"], "owner-a");
    assert_eq!(first_body["control_scope"], "process");

    let replay = client
        .post(format!("{}/flows/flow-a/run", replica_b.base))
        .header("Idempotency-Key", "replica-contract-run")
        .json(&serde_json::json!({"test": true}))
        .send()
        .await
        .unwrap();
    assert_eq!(replay.status(), reqwest::StatusCode::OK);
    assert_eq!(
        replay
            .headers()
            .get("Idempotency-Replayed")
            .and_then(|value| value.to_str().ok()),
        Some("true")
    );
    let replay_body: serde_json::Value = replay.json().await.unwrap();
    assert_eq!(replay_body, first_body);
    assert!(replica_b.state.active_runs.read().await.is_empty());

    let question = wait_for_question(&client, &replica_a, &run_id).await;
    let question_id = question["question_id"].as_str().unwrap();

    assert_foreign_owner(
        client
            .get(format!(
                "{}/flows/flow-a/questions/{run_id}",
                replica_b.base
            ))
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_foreign_owner(
        client
            .post(format!("{}/flows/flow-a/answer/{run_id}", replica_b.base))
            .json(&serde_json::json!({
                "question_id": question_id,
                "answer": "yes",
            }))
            .send()
            .await
            .unwrap(),
    )
    .await;
    // SQLite intentionally has no shared cancellation mailbox, so its
    // foreign abort remains an owner-aware conflict instead of pretending the
    // local process aborted the run.
    assert_foreign_owner(
        client
            .post(format!("{}/flows/flow-a/abort/{run_id}", replica_b.base))
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_foreign_owner(
        client
            .get(format!("{}/flows/flow-a/events/{run_id}", replica_b.base))
            .send()
            .await
            .unwrap(),
    )
    .await;

    let wrong_flow = client
        .get(format!(
            "{}/flows/flow-b/questions/{run_id}",
            replica_b.base
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(wrong_flow.status(), reqwest::StatusCode::NOT_FOUND);

    let delivered = client
        .post(format!("{}/flows/flow-a/answer/{run_id}", replica_a.base))
        .json(&serde_json::json!({
            "question_id": question_id,
            "answer": "yes",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(delivered.status(), reqwest::StatusCode::OK);
    assert_eq!(
        wait_for_terminal(&replica_b.store, &run_id).await,
        RunStatus::Success
    );

    let terminal_events = client
        .get(format!("{}/flows/flow-a/events/{run_id}", replica_b.base))
        .send()
        .await
        .unwrap();
    assert_eq!(terminal_events.status(), reqwest::StatusCode::OK);
    let terminal_events = terminal_events.text().await.unwrap();
    assert!(terminal_events.contains("event: run_complete"));
    assert!(terminal_events.contains("\"status\":\"success\""));

    let locally_owned_but_missing = uuid::Uuid::new_v4().to_string();
    replica_b
        .store
        .save_run_intent(RunIntent {
            suggested_id: Some(locally_owned_but_missing.clone()),
            flow_name: "local owner miss".into(),
            flow: "flow-a".into(),
            started_at: chrono::Utc::now().to_rfc3339(),
            agent_count: 0,
            task_count: 0,
            tags: vec![],
        })
        .await
        .unwrap();
    let unavailable = client
        .get(format!(
            "{}/flows/flow-a/questions/{locally_owned_but_missing}",
            replica_b.base
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(
        unavailable.status(),
        reqwest::StatusCode::SERVICE_UNAVAILABLE
    );
    let unavailable: serde_json::Value = unavailable.json().await.unwrap();
    assert_eq!(unavailable["code"], "run_control_temporarily_unavailable");
    assert_eq!(unavailable["owner_instance_id"], "owner-b");
    assert_eq!(unavailable["retryable"], true);
}
