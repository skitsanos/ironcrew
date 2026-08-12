//! HTTP regressions for persisted conversation agent identity.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use ironcrew::api::{AppState, create_router};
use ironcrew::engine::run_history::JsonFileStore;
use ironcrew::engine::sqlite_store::SqliteStore;
use ironcrew::engine::store::StateStore;

const CHAT_FLOW: &str = r#"
local crew = Crew.new({
    goal = "conversation identity test",
    provider = "openai",
    model = "test",
    api_key = "test",
})
crew:add_agent(Agent.new({
    name = "tutor",
    goal = "test persisted identity",
    system_prompt = "test",
}))
"#;

const AGENT_CONFLICT: &str =
    "start: requested `agent` does not match the stored conversation agent";

struct TestServer {
    base: String,
    state: Arc<AppState>,
    server: tokio::task::JoinHandle<()>,
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.server.abort();
    }
}

fn app_state(root: &Path, store: Arc<dyn StateStore>) -> Arc<AppState> {
    let rate_policy = ironcrew::api::admission::RatePolicy {
        rate_per_minute: 60_000,
        burst: 1_000,
    };
    let admission = ironcrew::api::admission::AdmissionController::new(
        ironcrew::api::admission::AdmissionConfig {
            work: rate_policy,
            control: rate_policy,
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
        max_active_conversations: 1,
        conversation_permits: Arc::new(tokio::sync::Semaphore::new(1)),
        max_active_runs: 1,
        run_permits: Arc::new(tokio::sync::Semaphore::new(1)),
        max_sse_connections: 1,
        sse_permits: Arc::new(tokio::sync::Semaphore::new(1)),
        max_run_lifetime: Duration::from_secs(60),
        terminal_persistence_failures: std::sync::atomic::AtomicUsize::new(0),
        store_maintenance_healthy: AtomicBool::new(true),
        readiness_cache: tokio::sync::Mutex::new(None),
        idempotency: Default::default(),
        store,
    })
}

async fn spawn_server(root: &Path, store: Arc<dyn StateStore>) -> TestServer {
    let state = app_state(root, store);
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
    TestServer {
        base: format!("http://{address}"),
        state,
        server,
    }
}

async fn assert_agent_conflict(response: reqwest::Response, backend: &str) {
    assert_eq!(
        response.status(),
        reqwest::StatusCode::CONFLICT,
        "{backend} status"
    );
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["error"], AGENT_CONFLICT, "{backend} error body");
}

async fn exercise_persisted_agent_contract(root: &Path, store: Arc<dyn StateStore>, backend: &str) {
    let flow_dir = root.join("chat");
    std::fs::create_dir_all(&flow_dir).unwrap();
    let flow_file = flow_dir.join("crew.lua");
    std::fs::write(&flow_file, CHAT_FLOW).unwrap();

    let server = spawn_server(root, store.clone()).await;
    let client = reqwest::Client::new();
    let start_url = format!(
        "{}/flows/chat/conversations/persisted-agent/start",
        server.base
    );

    let initial = client
        .post(&start_url)
        .json(&serde_json::json!({ "agent": "tutor" }))
        .send()
        .await
        .unwrap();
    assert_eq!(initial.status(), reqwest::StatusCode::OK, "{backend}");
    let persisted_before = store
        .get_conversation(Some("chat"), "persisted-agent")
        .await
        .unwrap()
        .unwrap();
    let persisted_before = serde_json::to_value(persisted_before).unwrap();

    // A wrong-agent start must not evaluate the flow, even while its existing
    // handle is live. The invalid script makes accidental construction visible.
    std::fs::write(&flow_file, "error('start must not evaluate this flow')").unwrap();
    let active_conflict = client
        .post(&start_url)
        .json(&serde_json::json!({ "agent": "other" }))
        .send()
        .await
        .unwrap();
    assert_agent_conflict(active_conflict, backend).await;
    assert_eq!(
        serde_json::to_value(
            store
                .get_conversation(Some("chat"), "persisted-agent")
                .await
                .unwrap()
                .unwrap()
        )
        .unwrap(),
        persisted_before,
        "{backend} active conflict mutated the durable conversation"
    );
    assert_eq!(server.state.active_conversations.read().await.len(), 1);
    assert_eq!(server.state.conversation_permits.available_permits(), 0);

    server
        .state
        .active_conversations
        .write()
        .await
        .remove(&("chat".to_string(), "persisted-agent".to_string()));
    assert_eq!(server.state.conversation_permits.available_permits(), 1);

    // Repeat with only the durable record present. A 409 rather than the Lua
    // error proves identity validation happens before rebuilding the session.
    let stored_conflict = client
        .post(&start_url)
        .json(&serde_json::json!({ "agent": "other" }))
        .send()
        .await
        .unwrap();
    assert_agent_conflict(stored_conflict, backend).await;
    assert!(server.state.active_conversations.read().await.is_empty());
    assert_eq!(server.state.conversation_permits.available_permits(), 1);
    assert_eq!(
        serde_json::to_value(
            store
                .get_conversation(Some("chat"), "persisted-agent")
                .await
                .unwrap()
                .unwrap()
        )
        .unwrap(),
        persisted_before,
        "{backend} stored conflict mutated the durable conversation"
    );

    std::fs::write(&flow_file, CHAT_FLOW).unwrap();
    let empty_resume = client
        .post(&start_url)
        .json(&serde_json::json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(empty_resume.status(), reqwest::StatusCode::OK, "{backend}");
    let body: serde_json::Value = empty_resume.json().await.unwrap();
    assert_eq!(body["agent"], "tutor", "{backend}");

    server
        .state
        .active_conversations
        .write()
        .await
        .remove(&("chat".to_string(), "persisted-agent".to_string()));
    let same_agent_resume = client
        .post(&start_url)
        .json(&serde_json::json!({ "agent": " tutor " }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        same_agent_resume.status(),
        reqwest::StatusCode::OK,
        "{backend}"
    );
    let body: serde_json::Value = same_agent_resume.json().await.unwrap();
    assert_eq!(body["agent"], "tutor", "{backend}");
}

#[tokio::test]
async fn persisted_agent_identity_is_enforced_by_json_and_sqlite_http_starts() {
    let temp = tempfile::tempdir().unwrap();

    let json_root = temp.path().join("json");
    let json_store: Arc<dyn StateStore> =
        Arc::new(JsonFileStore::new(json_root.join("state")).unwrap());
    exercise_persisted_agent_contract(&json_root, json_store, "json").await;

    let sqlite_root = temp.path().join("sqlite");
    std::fs::create_dir_all(&sqlite_root).unwrap();
    let sqlite_store: Arc<dyn StateStore> =
        Arc::new(SqliteStore::new(sqlite_root.join("state.sqlite")).unwrap());
    exercise_persisted_agent_contract(&sqlite_root, sqlite_store, "sqlite").await;
}
