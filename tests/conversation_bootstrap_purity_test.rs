//! HTTP conversation entrypoint discovery must be declarative and repeatable.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use ironcrew::api::{AppState, create_router};
use ironcrew::engine::run_history::JsonFileStore;
use ironcrew::engine::store::StateStore;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

struct Server {
    base_url: String,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

struct Probe {
    base_url: String,
    hits: Arc<AtomicUsize>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Probe {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn spawn_probe() -> Probe {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let hits = Arc::new(AtomicUsize::new(0));
    let task_hits = hits.clone();
    let task = tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            task_hits.fetch_add(1, Ordering::SeqCst);
            tokio::spawn(async move {
                let mut request = vec![0_u8; 32 * 1024];
                let _ = stream.read(&mut request).await;
                let body = r#"{"choices":[{"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}"#;
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.shutdown().await;
            });
        }
    });
    Probe {
        base_url: format!("http://{address}"),
        hits,
        task,
    }
}

fn write_flow(root: &Path, name: &str, script: &str) {
    let directory = root.join(name);
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::write(directory.join("crew.lua"), script).unwrap();
}

async fn spawn_server(root: &Path) -> Server {
    let store: Arc<dyn StateStore> =
        Arc::new(JsonFileStore::new(root.join("state")).expect("create bootstrap test store"));
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
        max_active_conversations: 8,
        conversation_permits: Arc::new(tokio::sync::Semaphore::new(8)),
        max_active_runs: 2,
        run_permits: Arc::new(tokio::sync::Semaphore::new(2)),
        max_sse_connections: 2,
        sse_permits: Arc::new(tokio::sync::Semaphore::new(2)),
        max_run_lifetime: Duration::from_secs(10),
        terminal_persistence_failures: AtomicUsize::new(0),
        store_maintenance_healthy: AtomicBool::new(true),
        readiness_cache: tokio::sync::Mutex::new(None),
        idempotency: Default::default(),
        store,
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        axum::serve(
            listener,
            create_router(state).into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap();
    });
    Server {
        base_url: format!("http://{address}"),
        task,
    }
}

async fn start(client: &reqwest::Client, server: &Server, flow: &str) -> reqwest::Response {
    tokio::time::timeout(
        Duration::from_secs(5),
        client
            .post(format!(
                "{}/flows/{flow}/conversations/session/start",
                server.base_url
            ))
            .json(&serde_json::json!({ "agent": "tutor" }))
            .send(),
    )
    .await
    .expect("conversation bootstrap request deadline")
    .expect("conversation bootstrap request")
}

async fn assert_blocked(response: reqwest::Response, capability: &str, phase: &str) {
    let status = response.status();
    let body: serde_json::Value = response.json().await.unwrap();
    let error = body["error"].as_str().unwrap();
    assert_eq!(
        status,
        reqwest::StatusCode::BAD_REQUEST,
        "unexpected error: {error}"
    );
    assert!(error.contains(capability), "unexpected error: {error}");
    assert!(error.contains(phase), "unexpected error: {error}");
}

fn crew_definition(goal: &str, extra: &str) -> String {
    format!(
        r#"
local crew = Crew.new({{
    goal = {goal:?},
    provider = "openai",
    model = "offline-test",
    api_key = "unused",
}})
crew:add_agent(Agent.new({{
    name = "tutor",
    goal = "test bootstrap purity",
    system_prompt = "test",
}}))
{extra}
"#
    )
}

#[tokio::test]
async fn http_conversation_bootstrap_blocks_effects_but_allows_declarative_start() {
    let workspace = tempfile::tempdir().unwrap();
    let root = workspace.path();
    let probe = spawn_probe().await;

    write_flow(
        root,
        "http-effect",
        &format!(
            "http.get({:?})\n{}",
            format!("{}/top-level", probe.base_url),
            crew_definition("http effect", "")
        ),
    );

    let subflow_dir = root.join("subflow-effect");
    std::fs::create_dir_all(&subflow_dir).unwrap();
    std::fs::write(
        subflow_dir.join("child.lua"),
        format!("http.get({:?})\nreturn {{ ok = true }}", probe.base_url),
    )
    .unwrap();
    std::fs::write(
        subflow_dir.join("crew.lua"),
        format!(
            "run_flow('child.lua', {{}})\n{}",
            crew_definition("subflow effect", "")
        ),
    )
    .unwrap();

    let run_script = format!(
        r#"
local crew = Crew.new({{
    goal = "run effect",
    provider = "openai",
    model = "offline-test",
    base_url = {:?},
    api_key = "unused",
}})
crew:add_agent(Agent.new({{
    name = "tutor",
    goal = "test bootstrap purity",
    system_prompt = "test",
}}))
crew:add_task({{
    name = "answer",
    description = "Return ok",
    expected_output = "ok",
    agent = "tutor",
}})
crew:run()
"#,
        probe.base_url
    );
    write_flow(root, "run-effect", &run_script);

    let memory_script = crew_definition(
        "memory effect",
        "crew:memory_set('bootstrap', 'must not be written')",
    )
    .replace(
        "api_key = \"unused\",",
        "api_key = \"unused\",\n    memory = \"persistent\",",
    );
    write_flow(root, "memory-effect", &memory_script);
    write_flow(
        root,
        "declarative",
        &crew_definition("declarative start", ""),
    );

    write_flow(root, "config-effect", &crew_definition("config effect", ""));
    std::fs::write(
        root.join("config-effect/config.lua"),
        format!("http.get({:?})\nreturn {{}}", probe.base_url),
    )
    .unwrap();

    let definition_dir = root.join("definition-effect/agents");
    std::fs::create_dir_all(&definition_dir).unwrap();
    std::fs::write(
        definition_dir.join("tutor.lua"),
        format!(
            "http.get({:?})\nreturn {{ name = 'tutor', goal = 'definition purity', system_prompt = 'test' }}",
            probe.base_url
        ),
    )
    .unwrap();
    write_flow(
        root,
        "definition-effect",
        r#"
local crew = Crew.new({
    goal = "definition effect",
    provider = "openai",
    model = "offline-test",
    api_key = "unused",
})
        "#,
    );

    write_flow(
        root,
        "tool-definition-effect",
        &crew_definition("tool definition effect", ""),
    );
    let tool_dir = root.join("tool-definition-effect/tools");
    std::fs::create_dir_all(&tool_dir).unwrap();
    std::fs::write(
        tool_dir.join("probe.lua"),
        format!(
            r#"
http.get({:?})
return {{
    name = "probe",
    description = "definition purity probe",
    parameters = {{}},
    execute = function() return "ok" end,
}}
"#,
            probe.base_url
        ),
    )
    .unwrap();
    write_flow(
        root,
        "normal-runtime",
        &format!("http.get({:?})", probe.base_url),
    );

    let server = spawn_server(root).await;
    let client = reqwest::Client::new();

    assert_blocked(
        start(&client, &server, "http-effect").await,
        "http.get",
        "HTTP conversation bootstrap",
    )
    .await;
    assert_blocked(
        start(&client, &server, "subflow-effect").await,
        "run_flow",
        "HTTP conversation bootstrap",
    )
    .await;
    assert_blocked(
        start(&client, &server, "run-effect").await,
        "crew:run",
        "HTTP conversation bootstrap",
    )
    .await;
    assert_blocked(
        start(&client, &server, "memory-effect").await,
        "crew:memory_set",
        "HTTP conversation bootstrap",
    )
    .await;
    assert_blocked(
        start(&client, &server, "config-effect").await,
        "http.get",
        "config.lua evaluation",
    )
    .await;
    assert_blocked(
        start(&client, &server, "definition-effect").await,
        "http.get",
        "HTTP conversation bootstrap",
    )
    .await;
    assert_blocked(
        start(&client, &server, "tool-definition-effect").await,
        "http.get",
        "HTTP conversation bootstrap",
    )
    .await;

    assert_eq!(
        probe.hits.load(Ordering::SeqCst),
        0,
        "bootstrap reached a network or provider effect"
    );
    assert!(
        !root.join("memory-effect/.ironcrew/memory.json").exists(),
        "bootstrap created persistent Crew memory"
    );

    let declarative = start(&client, &server, "declarative").await;
    assert_eq!(declarative.status(), reqwest::StatusCode::OK);

    let normal_path = root.join("normal-runtime");
    let loader = ironcrew::cli::project::load_project(&normal_path).unwrap();
    let (lua, _runtime) = ironcrew::cli::project::setup_crew_runtime(&loader).unwrap();
    let script = std::fs::read_to_string(normal_path.join("crew.lua")).unwrap();
    let normal_error = lua
        .load(&script)
        .exec_async()
        .await
        .unwrap_err()
        .to_string();
    assert!(
        normal_error.contains("SSRF blocked")
            && !normal_error.contains("HTTP conversation bootstrap"),
        "ordinary runtime setup should retain the normal HTTP capability boundary: {normal_error}"
    );
}
