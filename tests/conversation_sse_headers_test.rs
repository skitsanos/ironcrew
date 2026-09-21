//! HTTP boundary coverage for sensitive conversation SSE responses.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use ironcrew::api::{AppState, create_router};
use ironcrew::engine::run_history::JsonFileStore;
use ironcrew::engine::store::StateStore;

const CHAT_FLOW: &str = r#"
local crew = Crew.new({
    goal = "conversation SSE header test",
    provider = "openai",
    model = "offline-test",
    api_key = "unused",
})
crew:add_agent(Agent.new({
    name = "tutor",
    goal = "test conversation event transport",
    system_prompt = "test",
}))
"#;

struct TestServer {
    base_url: String,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn spawn_server(root: &Path) -> TestServer {
    let flow_dir = root.join("chat");
    std::fs::create_dir_all(&flow_dir).expect("create conversation SSE fixture");
    std::fs::write(flow_dir.join("crew.lua"), CHAT_FLOW).expect("write conversation SSE fixture");
    let store: Arc<dyn StateStore> =
        Arc::new(JsonFileStore::new(root.join("state")).expect("create conversation SSE store"));
    let roomy = ironcrew::api::admission::RatePolicy {
        rate_per_minute: 60_000,
        burst: 1_000,
    };
    let state = Arc::new(AppState {
        flows_dir: root.to_path_buf(),
        runtime_identity: ironcrew::api::deployment::RuntimeIdentity::disabled(),
        auth: Arc::new(ironcrew::api::auth::AuthConfig::disabled()),
        admission: Arc::new(ironcrew::api::admission::AdmissionController::new(
            ironcrew::api::admission::AdmissionConfig {
                work: roomy,
                control: roomy,
            },
        )),
        lifecycle: ironcrew::api::lifecycle::LifecycleController::new(),
        active_runs: Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new())),
        active_conversations: Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new())),
        conversation_lifecycles: Arc::new(
            ironcrew::api::conversation_lifecycle::ConversationLifecycleRegistry::new(
                ironcrew::api::conversation_lifecycle::max_active_conversation_lifecycles(),
            ),
        ),
        max_active_conversations: 1,
        conversation_permits: Arc::new(tokio::sync::Semaphore::new(1)),
        max_active_runs: 1,
        run_permits: Arc::new(tokio::sync::Semaphore::new(1)),
        max_active_inspections: 1,
        inspection_permits: Arc::new(tokio::sync::Semaphore::new(1)),
        max_sse_connections: 1,
        sse_permits: Arc::new(tokio::sync::Semaphore::new(1)),
        max_run_lifetime: Duration::from_secs(60),
        terminal_persistence_failures: std::sync::atomic::AtomicUsize::new(0),
        store_maintenance_healthy: AtomicBool::new(true),
        readiness_cache: tokio::sync::Mutex::new(None),
        idempotency: Default::default(),
        store,
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind conversation SSE server");
    let address = listener.local_addr().expect("conversation SSE address");
    let task = tokio::spawn(async move {
        axum::serve(
            listener,
            create_router(state).into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .expect("serve conversation SSE fixture");
    });
    TestServer {
        base_url: format!("http://{address}"),
        task,
    }
}

async fn send(request: reqwest::RequestBuilder) -> reqwest::Response {
    tokio::time::timeout(Duration::from_secs(5), request.send())
        .await
        .expect("conversation SSE request deadline")
        .expect("conversation SSE HTTP request")
}

#[tokio::test]
async fn conversation_sse_success_and_errors_are_hardened() {
    let workspace = tempfile::tempdir().expect("conversation SSE workspace");
    let server = spawn_server(workspace.path()).await;
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(2))
        .build()
        .expect("build conversation SSE client");
    let events_url = format!(
        "{}/flows/chat/conversations/session/events",
        server.base_url
    );

    let started = send(
        client
            .post(format!(
                "{}/flows/chat/conversations/session/start",
                server.base_url
            ))
            .json(&serde_json::json!({ "agent": "tutor" })),
    )
    .await;
    assert_eq!(started.status(), reqwest::StatusCode::OK);

    let cursor = send(client.get(&events_url).header("Last-Event-ID", "1")).await;
    assert_eq!(cursor.status(), reqwest::StatusCode::CONFLICT);
    assert_eq!(cursor.headers()[reqwest::header::CACHE_CONTROL], "no-store");
    let cursor: serde_json::Value = cursor.json().await.expect("parse cursor boundary");
    assert!(
        cursor["error"]
            .as_str()
            .is_some_and(|message| message.contains("does not support Last-Event-ID"))
    );

    let stream = send(client.get(&events_url)).await;
    assert_eq!(stream.status(), reqwest::StatusCode::OK);
    assert_eq!(
        stream.headers()[reqwest::header::CONTENT_TYPE],
        "text/event-stream"
    );
    assert_eq!(
        stream.headers()[reqwest::header::CACHE_CONTROL],
        "no-store, no-transform"
    );
    assert_eq!(stream.headers()["x-accel-buffering"], "no");

    let saturated = send(client.get(&events_url)).await;
    assert_eq!(saturated.status(), reqwest::StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        saturated.headers()[reqwest::header::CACHE_CONTROL],
        "no-store"
    );
    assert_eq!(saturated.headers()[reqwest::header::RETRY_AFTER], "60");

    let malformed = send(client.get(format!(
        "{}/flows/chat/conversations/%FF/events",
        server.base_url
    )))
    .await;
    assert_eq!(malformed.status(), reqwest::StatusCode::BAD_REQUEST);
    assert_eq!(
        malformed.headers()[reqwest::header::CACHE_CONTROL],
        "no-store"
    );

    drop(stream);
}
