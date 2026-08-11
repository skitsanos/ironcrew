//! Production-safety regression tests for API admission, lifecycle, probes,
//! flow isolation, and terminal SSE delivery. Fixtures suspend before any LLM
//! call, so the suite is deterministic and provider-free.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use ironcrew::api::{AppState, create_router};
use ironcrew::engine::run_history::{JsonFileStore, RunStatus};
use ironcrew::engine::store::StateStore;
use ironcrew::llm::provider::ChatMessage;

const PARK_FLOW: &str = r#"
local crew = Crew.new({
    goal = "park for API safety test",
    provider = "openai",
    model = "test",
    api_key = "test",
})
crew:ask_human({ prompt = "Waiting", timeout_s = 600 })
"#;

const ERROR_FLOW: &str = r#"
error("intentional API safety test failure")
"#;

const CHAT_FLOW: &str = r#"
local crew = Crew.new({
    goal = "chat admission test",
    provider = "openai",
    model = "test",
    api_key = "test",
})
crew:add_agent(Agent.new({
    name = "tutor",
    goal = "test chat admission",
    system_prompt = "test",
}))
"#;

struct TestServer {
    base: String,
    state: Arc<AppState>,
    store: Arc<dyn StateStore>,
    root: std::path::PathBuf,
    _temp: tempfile::TempDir,
}

async fn spawn_server(
    max_runs: usize,
    max_conversations: usize,
    max_lifetime: Duration,
) -> TestServer {
    spawn_server_with_idempotency(
        max_runs,
        max_conversations,
        max_lifetime,
        Default::default(),
    )
    .await
}

async fn spawn_server_with_idempotency(
    max_runs: usize,
    max_conversations: usize,
    max_lifetime: Duration,
    idempotency: ironcrew::api::idempotency::IdempotencyConfig,
) -> TestServer {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().to_path_buf();
    for (name, script) in [
        ("flow-a", PARK_FLOW),
        ("flow-b", PARK_FLOW),
        ("error", ERROR_FLOW),
        ("chat", CHAT_FLOW),
    ] {
        let path = root.join(name);
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(path.join("crew.lua"), script).unwrap();
    }

    let store: Arc<dyn StateStore> =
        Arc::new(JsonFileStore::new(root.join(".ironcrew")).expect("create JSON test store"));
    // These regressions exercise run/session semaphores and durable
    // idempotency under synchronized bursts. Give that independent layer a
    // deliberately roomy admission policy; token-bucket behavior has focused
    // unit/HTTP coverage in `api::admission` and `api::auth`.
    let admission = ironcrew::api::admission::AdmissionController::new(
        ironcrew::api::admission::AdmissionConfig {
            work: ironcrew::api::admission::RatePolicy {
                rate_per_minute: 60_000,
                burst: 10_000,
            },
            control: ironcrew::api::admission::RatePolicy {
                rate_per_minute: 60_000,
                burst: 10_000,
            },
        },
    );
    let state = Arc::new(AppState {
        flows_dir: root.clone(),
        runtime_identity:
            ironcrew::api::deployment::RuntimeIdentity::try_new(Some(
                ironcrew::api::deployment::DeploymentEvidence {
                    revision: "test-revision".into(),
                    artifact_fingerprint:
                        "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
                            .into(),
                    flow_fingerprint:
                        "sha256:123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0"
                            .into(),
                    config_fingerprint:
                        "sha256:23456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef01"
                            .into(),
                    hitl_keyring_fingerprint:
                        "sha256:3456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef012"
                            .into(),
                },
            ))
            .unwrap(),
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
        max_active_conversations: max_conversations,
        conversation_permits: Arc::new(tokio::sync::Semaphore::new(max_conversations)),
        max_active_runs: max_runs,
        run_permits: Arc::new(tokio::sync::Semaphore::new(max_runs)),
        max_sse_connections: 100,
        sse_permits: Arc::new(tokio::sync::Semaphore::new(100)),
        max_run_lifetime: max_lifetime,
        terminal_persistence_failures: std::sync::atomic::AtomicUsize::new(0),
        store_maintenance_healthy: AtomicBool::new(true),
        readiness_cache: tokio::sync::Mutex::new(None),
        idempotency,
        store: store.clone(),
    });

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let app = create_router(state.clone());
    tokio::spawn(async move {
        let _ = axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await;
    });

    TestServer {
        base: format!("http://{address}"),
        state,
        store,
        root,
        _temp: temp,
    }
}

async fn start_run(client: &reqwest::Client, server: &TestServer, flow: &str) -> reqwest::Response {
    client
        .post(format!("{}/flows/{flow}/run", server.base))
        .json(&serde_json::json!({}))
        .send()
        .await
        .unwrap()
}

async fn wait_for_status(store: &Arc<dyn StateStore>, run_id: &str, expected: RunStatus) {
    for _ in 0..200 {
        if let Ok(record) = store.get_run(run_id).await
            && record.status == expected
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let observed = store.get_run(run_id).await.ok().map(|run| run.status);
    panic!("run {run_id} did not reach {expected}; observed {observed:?}");
}

#[tokio::test]
async fn concurrent_run_admission_never_exceeds_cap() {
    let server = Arc::new(spawn_server(1, 4, Duration::from_secs(60)).await);
    let client = reqwest::Client::new();
    let barrier = Arc::new(tokio::sync::Barrier::new(17));
    let mut requests = Vec::new();

    for _ in 0..16 {
        let server = server.clone();
        let client = client.clone();
        let barrier = barrier.clone();
        requests.push(tokio::spawn(async move {
            barrier.wait().await;
            start_run(&client, &server, "flow-a").await
        }));
    }
    barrier.wait().await;

    let responses = futures::future::join_all(requests).await;
    let mut accepted_run = None;
    let mut unavailable = 0;
    for response in responses {
        let response = response.unwrap();
        match response.status() {
            reqwest::StatusCode::OK => {
                let body: serde_json::Value = response.json().await.unwrap();
                assert!(
                    accepted_run
                        .replace(body["run_id"].as_str().unwrap().to_string())
                        .is_none()
                );
            }
            reqwest::StatusCode::SERVICE_UNAVAILABLE => unavailable += 1,
            status => panic!("unexpected run admission status: {status}"),
        }
    }

    assert_eq!(unavailable, 15);
    assert_eq!(server.state.run_permits.available_permits(), 0);

    let run_id = accepted_run.expect("one accepted run");
    let aborted = client
        .post(format!("{}/flows/flow-a/abort/{run_id}", server.base))
        .send()
        .await
        .unwrap();
    assert_eq!(aborted.status(), reqwest::StatusCode::OK);
    wait_for_status(&server.store, &run_id, RunStatus::Aborted).await;
    assert_eq!(server.state.run_permits.available_permits(), 1);
}

#[tokio::test]
async fn idempotent_run_replays_one_acceptance_and_rejects_key_reuse() {
    let server = spawn_server(1, 2, Duration::from_secs(60)).await;
    let client = reqwest::Client::new();
    let url = format!("{}/flows/flow-a/run", server.base);

    let first = client
        .post(&url)
        .header("Idempotency-Key", "run-replay-key")
        .json(&serde_json::json!({"input": "same"}))
        .send()
        .await
        .unwrap();
    assert_eq!(first.status(), reqwest::StatusCode::OK);
    assert!(first.headers().get("Idempotency-Replayed").is_none());
    let first_body: serde_json::Value = first.json().await.unwrap();
    let first_run_id = first_body["run_id"].as_str().unwrap();
    assert_eq!(first_body["owner_instance_id"], server.store.instance_id());
    assert_eq!(first_body["control_scope"], "process");
    assert!(
        server.store.get_run(first_run_id).await.is_ok(),
        "a keyed started response must never reference a run that is not durable"
    );

    let replay = client
        .post(&url)
        .header("Idempotency-Key", "run-replay-key")
        .json(&serde_json::json!({"input": "same"}))
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
    assert_eq!(server.state.active_runs.read().await.len(), 1);

    let conflict = client
        .post(&url)
        .header("Idempotency-Key", "run-replay-key")
        .json(&serde_json::json!({"input": "different"}))
        .send()
        .await
        .unwrap();
    assert_eq!(conflict.status(), reqwest::StatusCode::CONFLICT);

    let run_id = first_run_id;
    let aborted = client
        .post(format!("{}/flows/flow-a/abort/{run_id}", server.base))
        .send()
        .await
        .unwrap();
    assert_eq!(aborted.status(), reqwest::StatusCode::OK);
    wait_for_status(&server.store, run_id, RunStatus::Aborted).await;
}

#[tokio::test]
async fn production_policy_can_require_a_well_formed_idempotency_key() {
    let config = ironcrew::api::idempotency::IdempotencyConfig {
        require_key: true,
        ..Default::default()
    };
    let server = spawn_server_with_idempotency(1, 2, Duration::from_secs(60), config).await;
    let client = reqwest::Client::new();
    let url = format!("{}/flows/flow-a/run", server.base);

    let missing = client.post(&url).send().await.unwrap();
    assert_eq!(missing.status(), reqwest::StatusCode::BAD_REQUEST);
    let empty = client
        .post(&url)
        .header("Idempotency-Key", "")
        .send()
        .await
        .unwrap();
    assert_eq!(empty.status(), reqwest::StatusCode::BAD_REQUEST);

    let accepted = client
        .post(&url)
        .header("Idempotency-Key", "required-run-key")
        .send()
        .await
        .unwrap();
    assert_eq!(accepted.status(), reqwest::StatusCode::OK);
    let body: serde_json::Value = accepted.json().await.unwrap();
    let run_id = body["run_id"].as_str().unwrap();
    let aborted = client
        .post(format!("{}/flows/flow-a/abort/{run_id}", server.base))
        .send()
        .await
        .unwrap();
    assert_eq!(aborted.status(), reqwest::StatusCode::OK);
    wait_for_status(&server.store, run_id, RunStatus::Aborted).await;
}

#[tokio::test]
async fn concurrent_same_idempotency_key_starts_exactly_one_run() {
    let server = Arc::new(spawn_server(16, 2, Duration::from_secs(60)).await);
    let client = reqwest::Client::new();
    let barrier = Arc::new(tokio::sync::Barrier::new(17));
    let mut requests = Vec::new();
    for _ in 0..16 {
        let server = server.clone();
        let client = client.clone();
        let barrier = barrier.clone();
        requests.push(tokio::spawn(async move {
            barrier.wait().await;
            client
                .post(format!("{}/flows/flow-a/run", server.base))
                .header("Idempotency-Key", "concurrent-run-key")
                .json(&serde_json::json!({"batch": 1}))
                .send()
                .await
                .unwrap()
        }));
    }
    barrier.wait().await;

    let mut run_ids = std::collections::HashSet::new();
    let mut in_progress = 0;
    for response in futures::future::join_all(requests).await {
        let response = response.unwrap();
        match response.status() {
            reqwest::StatusCode::OK => {
                let body: serde_json::Value = response.json().await.unwrap();
                run_ids.insert(body["run_id"].as_str().unwrap().to_string());
            }
            reqwest::StatusCode::CONFLICT => in_progress += 1,
            status => panic!("unexpected idempotent run status: {status}"),
        }
    }
    assert!(in_progress < 16, "one request must own the durable claim");

    let replay = client
        .post(format!("{}/flows/flow-a/run", server.base))
        .header("Idempotency-Key", "concurrent-run-key")
        .json(&serde_json::json!({"batch": 1}))
        .send()
        .await
        .unwrap();
    assert_eq!(replay.status(), reqwest::StatusCode::OK);
    let replay_body: serde_json::Value = replay.json().await.unwrap();
    run_ids.insert(replay_body["run_id"].as_str().unwrap().to_string());
    assert_eq!(run_ids.len(), 1);
    assert_eq!(server.state.active_runs.read().await.len(), 1);

    let run_id = run_ids.into_iter().next().unwrap();
    let aborted = client
        .post(format!("{}/flows/flow-a/abort/{run_id}", server.base))
        .send()
        .await
        .unwrap();
    assert_eq!(aborted.status(), reqwest::StatusCode::OK);
    wait_for_status(&server.store, &run_id, RunStatus::Aborted).await;
}

#[tokio::test]
async fn concurrent_conversation_admission_never_exceeds_cap() {
    let server = Arc::new(spawn_server(2, 1, Duration::from_secs(60)).await);
    let client = reqwest::Client::new();
    let barrier = Arc::new(tokio::sync::Barrier::new(9));
    let mut requests = Vec::new();

    for index in 0..8 {
        let server = server.clone();
        let client = client.clone();
        let barrier = barrier.clone();
        requests.push(tokio::spawn(async move {
            barrier.wait().await;
            client
                .post(format!(
                    "{}/flows/chat/conversations/session-{index}/start",
                    server.base
                ))
                .json(&serde_json::json!({ "agent": "tutor" }))
                .send()
                .await
                .unwrap()
        }));
    }
    barrier.wait().await;

    let responses = futures::future::join_all(requests).await;
    let mut accepted_id = None;
    let mut unavailable = 0;
    for response in responses {
        let response = response.unwrap();
        match response.status() {
            reqwest::StatusCode::OK => {
                let body: serde_json::Value = response.json().await.unwrap();
                assert!(
                    accepted_id
                        .replace(body["conversation_id"].as_str().unwrap().to_string())
                        .is_none()
                );
            }
            reqwest::StatusCode::SERVICE_UNAVAILABLE => unavailable += 1,
            status => panic!("unexpected conversation admission status: {status}"),
        }
    }

    assert_eq!(unavailable, 7);
    assert_eq!(server.state.conversation_permits.available_permits(), 0);

    let accepted_id = accepted_id.expect("one accepted conversation");
    let deleted = client
        .delete(format!(
            "{}/flows/chat/conversations/{accepted_id}",
            server.base
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(deleted.status(), reqwest::StatusCode::OK);
    assert_eq!(server.state.conversation_permits.available_permits(), 1);
}

#[tokio::test]
async fn overlapping_conversation_message_fails_fast_instead_of_queueing() {
    let server = spawn_server(2, 1, Duration::from_secs(60)).await;
    let client = reqwest::Client::new();
    let started = client
        .post(format!(
            "{}/flows/chat/conversations/busy/start",
            server.base
        ))
        .json(&serde_json::json!({ "agent": "tutor" }))
        .send()
        .await
        .unwrap();
    assert_eq!(started.status(), reqwest::StatusCode::OK);

    let handle = server
        .state
        .active_conversations
        .read()
        .await
        .get(&("chat".to_string(), "busy".to_string()))
        .cloned()
        .expect("active conversation handle");
    let _active_turn = handle.turn_lock.lock().await;

    let response = tokio::time::timeout(
        Duration::from_secs(1),
        client
            .post(format!(
                "{}/flows/chat/conversations/busy/messages",
                server.base
            ))
            .json(&serde_json::json!({ "content": "do not queue" }))
            .send(),
    )
    .await
    .expect("busy response must not wait for the active turn")
    .unwrap();

    assert_eq!(response.status(), reqwest::StatusCode::CONFLICT);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(
        body["error"],
        "Conversation is busy; retry after the active operation completes"
    );
}

#[tokio::test]
async fn stale_durable_conversation_revision_invalidates_and_reloads_the_live_handle() {
    let server = spawn_server(2, 1, Duration::from_secs(60)).await;
    let client = reqwest::Client::new();
    let started = client
        .post(format!(
            "{}/flows/chat/conversations/stale-revision/start",
            server.base
        ))
        .json(&serde_json::json!({ "agent": "tutor" }))
        .send()
        .await
        .unwrap();
    assert_eq!(started.status(), reqwest::StatusCode::OK);

    // Simulate a second pod committing a newer durable transcript while this
    // pod still has the old Lua handle in memory.
    let mut durable = server
        .store
        .get_conversation(Some("chat"), "stale-revision")
        .await
        .unwrap()
        .unwrap();
    durable
        .messages
        .push(ChatMessage::user("committed elsewhere"));
    durable.messages.push(ChatMessage::assistant(
        Some("committed by another pod".into()),
        None,
    ));
    durable.updated_at = chrono::Utc::now().to_rfc3339();
    server.store.save_conversation(&durable).await.unwrap();

    let conflict = tokio::time::timeout(
        Duration::from_secs(2),
        client
            .post(format!(
                "{}/flows/chat/conversations/stale-revision/messages",
                server.base
            ))
            .header("Idempotency-Key", "stale-revision-message")
            .json(&serde_json::json!({ "content": "must not call the provider" }))
            .send(),
    )
    .await
    .expect("stale live-handle invalidation must not deadlock")
    .unwrap();
    assert_eq!(conflict.status(), reqwest::StatusCode::CONFLICT);
    assert!(
        !server
            .state
            .active_conversations
            .read()
            .await
            .contains_key(&("chat".to_string(), "stale-revision".to_string()))
    );

    let reopened = client
        .post(format!(
            "{}/flows/chat/conversations/stale-revision/start",
            server.base
        ))
        .json(&serde_json::json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(reopened.status(), reqwest::StatusCode::OK);
}

#[tokio::test]
async fn abort_is_persisted_and_terminal_sse_is_delivered_with_flow_isolation() {
    let server = spawn_server(2, 4, Duration::from_secs(60)).await;
    let client = reqwest::Client::new();
    let response = start_run(&client, &server, "flow-a").await;
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let body: serde_json::Value = response.json().await.unwrap();
    let run_id = body["run_id"].as_str().unwrap();

    let cross_flow = client
        .get(format!("{}/flows/flow-b/events/{run_id}", server.base))
        .send()
        .await
        .unwrap();
    assert_eq!(cross_flow.status(), reqwest::StatusCode::NOT_FOUND);

    let sse = client
        .get(format!("{}/flows/flow-a/events/{run_id}", server.base))
        .send()
        .await
        .unwrap();
    assert_eq!(sse.status(), reqwest::StatusCode::OK);

    let aborted = client
        .post(format!("{}/flows/flow-a/abort/{run_id}", server.base))
        .send()
        .await
        .unwrap();
    assert_eq!(aborted.status(), reqwest::StatusCode::OK);

    let stream_body = tokio::time::timeout(Duration::from_secs(5), sse.text())
        .await
        .expect("terminal SSE timeout")
        .unwrap();
    assert!(stream_body.contains("event: run_complete"));
    assert!(stream_body.contains("\"status\":\"aborted\""));
    wait_for_status(&server.store, run_id, RunStatus::Aborted).await;
}

#[tokio::test]
async fn timeout_and_pre_intent_error_are_persisted() {
    let server = spawn_server(2, 4, Duration::from_millis(50)).await;
    let client = reqwest::Client::new();

    let timed_out: serde_json::Value = start_run(&client, &server, "flow-a")
        .await
        .json()
        .await
        .unwrap();
    let timed_out_id = timed_out["run_id"].as_str().unwrap();
    wait_for_status(&server.store, timed_out_id, RunStatus::TimedOut).await;

    let failed: serde_json::Value = start_run(&client, &server, "error")
        .await
        .json()
        .await
        .unwrap();
    let failed_id = failed["run_id"].as_str().unwrap();
    wait_for_status(&server.store, failed_id, RunStatus::Failed).await;
}

#[tokio::test]
async fn terminal_sse_tombstones_never_exceed_run_admission_cap() {
    let server = spawn_server(1, 2, Duration::from_secs(60)).await;
    let client = reqwest::Client::new();

    for _ in 0..4 {
        let response = start_run(&client, &server, "error").await;
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        let body: serde_json::Value = response.json().await.unwrap();
        wait_for_status(
            &server.store,
            body["run_id"].as_str().unwrap(),
            RunStatus::Failed,
        )
        .await;
        for _ in 0..100 {
            if server.state.run_permits.available_permits() == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(server.state.active_runs.read().await.len() <= 1);
    }
}

#[tokio::test]
async fn conversation_api_rejects_unbounded_history_messages_and_images() {
    let server = spawn_server(2, 2, Duration::from_secs(60)).await;
    let client = reqwest::Client::new();

    let bad_history = client
        .post(format!(
            "{}/flows/chat/conversations/bad-history/start",
            server.base
        ))
        .json(&serde_json::json!({ "agent": "tutor", "max_history": 0 }))
        .send()
        .await
        .unwrap();
    assert_eq!(bad_history.status(), reqwest::StatusCode::BAD_REQUEST);

    let started = client
        .post(format!(
            "{}/flows/chat/conversations/bounded/start",
            server.base
        ))
        .json(&serde_json::json!({ "agent": "tutor" }))
        .send()
        .await
        .unwrap();
    assert_eq!(started.status(), reqwest::StatusCode::OK);

    let oversized_message = client
        .post(format!(
            "{}/flows/chat/conversations/bounded/messages",
            server.base
        ))
        .json(&serde_json::json!({ "content": "x".repeat(256 * 1024 + 1) }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        oversized_message.status(),
        reqwest::StatusCode::PAYLOAD_TOO_LARGE
    );

    let too_many_images = client
        .post(format!(
            "{}/flows/chat/conversations/bounded/messages",
            server.base
        ))
        .json(&serde_json::json!({
            "content": "hello",
            "images": ["a.png", "b.png", "c.png", "d.png", "e.png"]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        too_many_images.status(),
        reqwest::StatusCode::PAYLOAD_TOO_LARGE
    );
}

#[tokio::test]
async fn liveness_stays_up_while_readiness_checks_shutdown_flows_and_storage() {
    let server = spawn_server(1, 4, Duration::from_secs(60)).await;
    let client = reqwest::Client::new();

    for path in ["/health", "/health/live", "/health/ready"] {
        let response = client
            .get(format!("{}{}", server.base, path))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK, "{path}");
    }

    server.state.lifecycle.begin_fencing();
    let not_ready = client
        .get(format!("{}/health/ready", server.base))
        .send()
        .await
        .unwrap();
    assert_eq!(not_ready.status(), reqwest::StatusCode::SERVICE_UNAVAILABLE);
    let still_live = client
        .get(format!("{}/health/live", server.base))
        .send()
        .await
        .unwrap();
    assert_eq!(still_live.status(), reqwest::StatusCode::OK);

    let storage_server = spawn_server(1, 4, Duration::from_secs(60)).await;
    std::fs::remove_dir_all(storage_server.root.join(".ironcrew/runs")).unwrap();
    // Force a fresh backend probe: production readiness snapshots are cached
    // briefly to prevent probe storms from multiplying storage round trips.
    *storage_server.state.readiness_cache.lock().await = None;
    let storage_down = client
        .get(format!("{}/health/ready", storage_server.base))
        .send()
        .await
        .unwrap();
    assert_eq!(
        storage_down.status(),
        reqwest::StatusCode::SERVICE_UNAVAILABLE
    );
    let body: serde_json::Value = storage_down.json().await.unwrap();
    assert_eq!(body["component"], "storage");
}

#[tokio::test]
async fn overlapping_readiness_probes_coalesce_without_a_false_503() {
    let server = spawn_server(1, 4, Duration::from_secs(60)).await;
    let cache_guard = server.state.readiness_cache.lock().await;
    let client = reqwest::Client::new();
    let request = tokio::spawn({
        let url = format!("{}/health/ready", server.base);
        async move { client.get(url).send().await.unwrap() }
    });

    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        !request.is_finished(),
        "an overlapping healthy probe must wait for the in-flight check"
    );
    drop(cache_guard);

    let response = tokio::time::timeout(Duration::from_secs(1), request)
        .await
        .expect("coalesced readiness request must finish")
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["status"], "ready");
    assert_eq!(body["component"], "storage");
}

#[tokio::test]
async fn queued_readiness_probe_observes_a_drain_transition() {
    let server = spawn_server(1, 4, Duration::from_secs(60)).await;
    let cache_guard = server.state.readiness_cache.lock().await;
    let client = reqwest::Client::new();
    let request = tokio::spawn({
        let url = format!("{}/health/ready", server.base);
        async move { client.get(url).send().await.unwrap() }
    });

    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        !request.is_finished(),
        "the probe must be queued behind the active readiness check"
    );
    server.state.lifecycle.begin_fencing();
    drop(cache_guard);

    let response = tokio::time::timeout(Duration::from_secs(1), request)
        .await
        .expect("the queued readiness request must finish")
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::SERVICE_UNAVAILABLE);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["status"], "not_ready");
    assert_eq!(body["component"], "lifecycle");
    assert_eq!(body["lifecycle_state"], "fencing");
}

#[tokio::test]
async fn overlapping_readiness_probe_fails_closed_when_the_check_stalls() {
    let server = spawn_server(1, 4, Duration::from_secs(60)).await;
    let _cache_guard = server.state.readiness_cache.lock().await;
    let started = std::time::Instant::now();
    let response = tokio::time::timeout(
        Duration::from_secs(3),
        reqwest::Client::new()
            .get(format!("{}/health/ready", server.base))
            .send(),
    )
    .await
    .expect("a stalled coalesced probe must fail closed within its bound")
    .unwrap();

    assert_eq!(response.status(), reqwest::StatusCode::SERVICE_UNAVAILABLE);
    assert!(
        started.elapsed() >= Duration::from_millis(900),
        "a contended probe should coalesce before failing closed"
    );
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["status"], "not_ready");
    assert_eq!(body["component"], "readiness_check");
}

#[tokio::test]
async fn capabilities_reports_process_scoped_live_control() {
    let server = spawn_server(1, 1, Duration::from_secs(60)).await;
    let response = reqwest::get(format!("{}/capabilities", server.base))
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get(reqwest::header::CACHE_CONTROL)
            .and_then(|value| value.to_str().ok()),
        Some("no-store")
    );
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["instance_id"], server.store.instance_id());
    assert!(
        uuid::Uuid::parse_str(body["process_start_id"].as_str().unwrap()).is_ok(),
        "capabilities must expose a per-process start identity"
    );
    assert_eq!(body["deployment"]["revision"], "test-revision");
    assert_eq!(
        body["deployment"]["artifact_fingerprint"],
        "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
    );
    assert_eq!(
        body["deployment"]["hitl_keyring_fingerprint"],
        "sha256:3456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef012"
    );
    assert_eq!(body["topology"], "single_executor");
    assert_eq!(body["control_scope"], "process");
    assert_eq!(body["multi_replica_control"], false);
    assert_eq!(body["live_control"]["human_input"], "process");
    assert_eq!(body["live_control"]["sse_replay"], "process");
    assert_eq!(
        body["live_control"]["run_abort"]["cross_instance"],
        "keyed_store_if_supported"
    );
}
