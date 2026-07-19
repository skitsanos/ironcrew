//! End-to-end HTTP tests for the mid-run human-input endpoints:
//!   GET  /flows/{flow}/questions/{run_id}
//!   POST /flows/{flow}/answer/{run_id}
//!
//! Spins up a real axum server (same wiring as production, mirroring
//! http_audit_test.rs) with flows that suspend on `crew:ask_human()`. The
//! post-crew fixture uses a conditionally skipped task, so it exercises the
//! full `crew:run()` lifecycle without making an LLM request.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use ironcrew::api::{AppState, create_router};
use ironcrew::engine::run_history::JsonFileStore;
use ironcrew::engine::store::{RunLeaseConfig, StateStore};

/// Flow that suspends on ask_human and validates the delivered answer.
const ASK_FLOW: &str = r#"
local crew = Crew.new({
    goal = "ask-human e2e",
    provider = "openai",
    model = "test",
    api_key = "test",
})
local answer = crew:ask_human({
    prompt = "Ship it?",
    choices = { "yes", "no" },
    timeout_s = 30,
})
if answer ~= "yes" then
    error("unexpected answer: " .. tostring(answer))
end
print("answer accepted")
"#;

/// Flow that parks forever (long timeout) — abort-while-waiting fixture.
const PARK_FLOW: &str = r#"
local crew = Crew.new({
    goal = "park",
    provider = "openai",
    model = "test",
    api_key = "test",
})
crew:ask_human({ prompt = "Waiting forever", timeout_s = 600 })
"#;

/// Flow that completes its crew tasks before suspending in outer Lua. The
/// false condition makes the task deterministic and provider-free while still
/// producing a real task result for terminal persistence.
const POST_CREW_FLOW: &str = r#"
local crew = Crew.new({
    goal = "post-crew human checkpoint",
    provider = "openai",
    model = "test",
    api_key = "test",
})
crew:add_agent(Agent.new({
    name = "offline",
    goal = "Exercise lifecycle without a provider call",
    capabilities = { "testing" },
}))
crew:add_task_if("false", {
    name = "skipped",
    agent = "offline",
    description = "This task is intentionally skipped",
    expected_output = "No provider output",
})
local results = crew:run()
if not results[1] or not results[1].success then
    error("expected the conditionally skipped task result")
end
local answer = crew:ask_human({
    prompt = "Release the completed crew output?",
    choices = { "yes", "no" },
    timeout_s = 30,
})
if answer ~= "yes" then
    error("unexpected post-crew answer: " .. tostring(answer))
end
"#;

async fn spawn_test_server() -> (SocketAddr, PathBuf) {
    // SAFETY: same rationale as http_audit_test.rs — these tests want the
    // API token unset; the remove is idempotent.
    unsafe { std::env::remove_var("IRONCREW_API_TOKEN") };

    let temp = tempfile::tempdir().unwrap();
    let flows_dir = temp.path().to_path_buf();

    // Flow projects under the flows root.
    for (name, script) in [
        ("askflow", ASK_FLOW),
        ("parkflow", PARK_FLOW),
        ("postcrew", POST_CREW_FLOW),
    ] {
        let dir = flows_dir.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("crew.lua"), script).unwrap();
    }

    let ironcrew_dir = flows_dir.join(".ironcrew");
    std::fs::create_dir_all(&ironcrew_dir).unwrap();
    let lease = RunLeaseConfig::new(
        format!("ask-human-http-test-{}", uuid::Uuid::new_v4()),
        std::time::Duration::from_secs(3),
    )
    .unwrap();
    let store: Arc<dyn StateStore> =
        Arc::new(JsonFileStore::new_with_lease_config(ironcrew_dir, lease).unwrap());

    let _ = Box::leak(Box::new(temp));

    let state = Arc::new(AppState {
        flows_dir: flows_dir.clone(),
        auth: Arc::new(ironcrew::api::auth::AuthConfig::disabled()),
        admission: Arc::new(ironcrew::api::admission::AdmissionController::default()),
        accepting_traffic: std::sync::atomic::AtomicBool::new(true),
        active_runs: Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new())),
        active_conversations: Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new())),
        max_active_conversations: 100,
        conversation_permits: Arc::new(tokio::sync::Semaphore::new(100)),
        max_active_runs: 100,
        run_permits: Arc::new(tokio::sync::Semaphore::new(100)),
        max_sse_connections: 100,
        sse_permits: Arc::new(tokio::sync::Semaphore::new(100)),
        max_run_lifetime: std::time::Duration::from_secs(30 * 60),
        terminal_persistence_failures: std::sync::atomic::AtomicUsize::new(0),
        store_maintenance_healthy: std::sync::atomic::AtomicBool::new(true),
        readiness_cache: tokio::sync::Mutex::new(None),
        idempotency: Default::default(),
        store,
    });

    let app = create_router(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await;
    });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    (addr, flows_dir)
}

/// Poll the questions endpoint until a question appears (or panic after ~5s).
async fn wait_for_question(
    client: &reqwest::Client,
    base: &str,
    flow: &str,
    run_id: &str,
) -> serde_json::Value {
    for _ in 0..100 {
        let resp = client
            .get(format!("{}/flows/{}/questions/{}", base, flow, run_id))
            .send()
            .await
            .unwrap();
        if resp.status() == 200 {
            let body: serde_json::Value = resp.json().await.unwrap();
            if let Some(q) = body["questions"].as_array().and_then(|a| a.first()) {
                assert_eq!(body["status"], "waiting_for_input");
                return q.clone();
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("question never appeared for run {}", run_id);
}

#[tokio::test]
async fn happy_path_question_lifecycle_over_http() {
    let (addr, _flows) = spawn_test_server().await;
    let base = format!("http://{}", addr);
    let client = reqwest::Client::new();

    // Start the flow; it suspends on ask_human.
    let start: serde_json::Value = client
        .post(format!("{}/flows/askflow/run", base))
        .json(&serde_json::json!({}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let run_id = start["run_id"].as_str().expect("run_id").to_string();

    // The question surfaces on the questions endpoint with full metadata.
    let q = wait_for_question(&client, &base, "askflow", &run_id).await;
    assert_eq!(q["prompt"], "Ship it?");
    assert_eq!(q["choices"], serde_json::json!(["yes", "no"]));
    assert_eq!(q["timeout_s"], 30);
    let question_id = q["question_id"].as_str().unwrap().to_string();

    // An oversized answer is a retryable payload error, not a misleading
    // "question not found" response, and leaves the question pending.
    let resp = client
        .post(format!("{}/flows/askflow/answer/{}", base, run_id))
        .json(&serde_json::json!({
            "question_id": question_id,
            "answer": "x".repeat(64 * 1024),
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 413);
    let pending = wait_for_question(&client, &base, "askflow", &run_id).await;
    assert_eq!(pending["question_id"], question_id);

    // Deliver the answer.
    let resp = client
        .post(format!("{}/flows/askflow/answer/{}", base, run_id))
        .json(&serde_json::json!({ "question_id": question_id, "answer": "yes" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["status"], "delivered");

    // A repeat answer is a 404 — first writer won, the question is gone.
    let resp = client
        .post(format!("{}/flows/askflow/answer/{}", base, run_id))
        .json(&serde_json::json!({ "question_id": question_id, "answer": "no" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);

    // The flow resumes with the answer and completes successfully — the SSE
    // channel carries human_input_requested/received and run_complete.
    let events_text = client
        .get(format!("{}/flows/askflow/events/{}", base, run_id))
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
        .unwrap();
    // Read the SSE body until run_complete shows up (replay buffer makes
    // earlier events available even though we subscribed late).
    let text = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        let mut collected = String::new();
        let mut stream = events_text;
        loop {
            let chunk = stream.chunk().await.unwrap();
            let Some(chunk) = chunk else { break collected };
            collected.push_str(&String::from_utf8_lossy(&chunk));
            if collected.contains("run_complete") {
                break collected;
            }
        }
    })
    .await
    .expect("SSE stream never delivered run_complete");

    assert!(
        text.contains("human_input_requested"),
        "SSE missing human_input_requested: {text}"
    );
    assert!(
        text.contains("human_input_received"),
        "SSE missing human_input_received: {text}"
    );
    assert!(
        !text.contains("\"answer\""),
        "SSE must not echo the answer content: {text}"
    );

    // Terminal replay keeps the ActiveRun tombstone briefly, but its question
    // transport is already closed when run_complete is emitted.
    let resp = client
        .get(format!("{}/flows/askflow/questions/{}", base, run_id))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
}

#[tokio::test]
async fn post_crew_question_keeps_keyed_run_in_flight_until_outer_lua_finishes() {
    let (addr, _flows) = spawn_test_server().await;
    let base = format!("http://{}", addr);
    let client = reqwest::Client::new();

    let start = client
        .post(format!("{}/flows/postcrew/run", base))
        .header("Idempotency-Key", "post-crew-human-checkpoint")
        .json(&serde_json::json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(start.status(), 200);
    let start: serde_json::Value = start.json().await.unwrap();
    let run_id = start["run_id"].as_str().expect("run_id").to_string();

    let question = wait_for_question(&client, &base, "postcrew", &run_id).await;
    assert_eq!(question["prompt"], "Release the completed crew output?");
    let question_id = question["question_id"].as_str().unwrap().to_string();

    // `crew:run()` has returned, but the outer Lua entrypoint is still parked.
    // Its rich completion must be staged rather than terminalizing the durable
    // record (which would make the keyed-run heartbeat fence this worker).
    let record: serde_json::Value = client
        .get(format!("{}/flows/postcrew/runs/{}", base, run_id))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(record["status"], "WaitingForInput");
    assert_eq!(record["task_results"], serde_json::json!([]));

    // The test store has a three-second run lease, so this crosses a keyed-run
    // heartbeat tick. A prematurely terminal record would abort the Lua worker
    // and expire this question during the wait.
    tokio::time::sleep(std::time::Duration::from_millis(1_500)).await;
    let still_pending = wait_for_question(&client, &base, "postcrew", &run_id).await;
    assert_eq!(still_pending["question_id"], question_id);

    let answer = client
        .post(format!("{}/flows/postcrew/answer/{}", base, run_id))
        .json(&serde_json::json!({
            "question_id": question_id,
            "answer": "yes",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(answer.status(), 200);

    let terminal = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let record: serde_json::Value = client
                .get(format!("{}/flows/postcrew/runs/{}", base, run_id))
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            if record["status"] == "Success" {
                break record;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("post-crew run did not become terminal after its answer");

    let results = terminal["task_results"].as_array().unwrap();
    assert_eq!(results.len(), 1, "staged task results were not persisted");
    assert_eq!(results[0]["task"], "skipped");
    assert_eq!(results[0]["success"], true);
    assert!(
        terminal["duration_ms"].as_u64().unwrap() >= 1_400,
        "HTTP run duration did not include the outer-Lua input wait"
    );
}

#[tokio::test]
async fn abort_after_crew_completion_preserves_staged_task_results() {
    let (addr, _flows) = spawn_test_server().await;
    let base = format!("http://{}", addr);
    let client = reqwest::Client::new();

    let start: serde_json::Value = client
        .post(format!("{}/flows/postcrew/run", base))
        .header("Idempotency-Key", "abort-post-crew-human-checkpoint")
        .json(&serde_json::json!({}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let run_id = start["run_id"].as_str().expect("run_id").to_string();
    wait_for_question(&client, &base, "postcrew", &run_id).await;

    let aborted = client
        .post(format!("{}/flows/postcrew/abort/{}", base, run_id))
        .send()
        .await
        .unwrap();
    assert_eq!(aborted.status(), 200);

    let terminal = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let record: serde_json::Value = client
                .get(format!("{}/flows/postcrew/runs/{}", base, run_id))
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            if record["status"] == "Aborted" {
                break record;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("post-crew run did not persist its abort");

    let results = terminal["task_results"].as_array().unwrap();
    assert_eq!(results.len(), 1, "abort discarded completed task results");
    assert_eq!(results[0]["task"], "skipped");
    assert_eq!(results[0]["success"], true);
    assert_eq!(terminal["total_tokens"], 0);
}

#[tokio::test]
async fn flow_scoping_hides_other_flows_runs() {
    let (addr, _flows) = spawn_test_server().await;
    let base = format!("http://{}", addr);
    let client = reqwest::Client::new();

    let start: serde_json::Value = client
        .post(format!("{}/flows/askflow/run", base))
        .json(&serde_json::json!({}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let run_id = start["run_id"].as_str().unwrap().to_string();
    let q = wait_for_question(&client, &base, "askflow", &run_id).await;
    let question_id = q["question_id"].as_str().unwrap().to_string();

    // Querying/answering the run through ANOTHER flow's URL is a 404 — the
    // endpoint must not confirm the run exists under a different flow.
    let resp = client
        .get(format!("{}/flows/parkflow/questions/{}", base, run_id))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
    let resp = client
        .post(format!("{}/flows/parkflow/answer/{}", base, run_id))
        .json(&serde_json::json!({ "question_id": question_id, "answer": "yes" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);

    // Clean up: answer through the right flow.
    let resp = client
        .post(format!("{}/flows/askflow/answer/{}", base, run_id))
        .json(&serde_json::json!({ "question_id": question_id, "answer": "yes" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
}

#[tokio::test]
async fn abort_while_waiting_kills_run_and_expires_questions() {
    let (addr, _flows) = spawn_test_server().await;
    let base = format!("http://{}", addr);
    let client = reqwest::Client::new();

    let start: serde_json::Value = client
        .post(format!("{}/flows/parkflow/run", base))
        .json(&serde_json::json!({}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let run_id = start["run_id"].as_str().unwrap().to_string();
    let q = wait_for_question(&client, &base, "parkflow", &run_id).await;
    let question_id = q["question_id"].as_str().unwrap().to_string();

    // Abort the suspended run.
    let resp = client
        .post(format!("{}/flows/parkflow/abort/{}", base, run_id))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    // Abort closes the question transport before returning even though the
    // ActiveRun tombstone and its event bus remain available for SSE replay.
    let resp = client
        .get(format!("{}/flows/parkflow/questions/{}", base, run_id))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);

    let resp = client
        .post(format!("{}/flows/parkflow/answer/{}", base, run_id))
        .json(&serde_json::json!({ "question_id": question_id, "answer": "x" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);

    let mut events = client
        .get(format!("{}/flows/parkflow/events/{}", base, run_id))
        .send()
        .await
        .unwrap();
    assert_eq!(events.status(), 200);
    let text = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        let mut collected = String::new();
        loop {
            let Some(chunk) = events.chunk().await.unwrap() else {
                break collected;
            };
            collected.push_str(&String::from_utf8_lossy(&chunk));
            if collected.contains("run_complete") {
                break collected;
            }
        }
    })
    .await
    .expect("SSE stream never delivered run_complete after abort");
    assert!(
        text.contains("run_complete"),
        "SSE missing terminal event: {text}"
    );
    assert!(
        text.contains("aborted"),
        "SSE missing aborted status: {text}"
    );

    // unknown question id on a live run is also 404 (separate start to
    // check the discrimination is per-question, not per-run)
    let start2: serde_json::Value = client
        .post(format!("{}/flows/parkflow/run", base))
        .json(&serde_json::json!({}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let run2 = start2["run_id"].as_str().unwrap().to_string();
    wait_for_question(&client, &base, "parkflow", &run2).await;
    let resp = client
        .post(format!("{}/flows/parkflow/answer/{}", base, run2))
        .json(&serde_json::json!({ "question_id": "nope", "answer": "x" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
}
