//! End-to-end HTTP tests for the mid-run human-input endpoints:
//!   GET  /flows/{flow}/questions/{run_id}
//!   POST /flows/{flow}/answer/{run_id}
//!
//! Spins up a real axum server (same wiring as production, mirroring
//! http_audit_test.rs) with a flows dir containing a crew.lua that suspends
//! on `crew:ask_human()`. The flow never calls `crew:run()`, so no LLM
//! access is needed.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use ironcrew::api::{AppState, create_router};
use ironcrew::engine::store::create_store;

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

async fn spawn_test_server() -> (SocketAddr, PathBuf) {
    // SAFETY: same rationale as http_audit_test.rs — these tests want the
    // API token unset; the remove is idempotent.
    unsafe { std::env::remove_var("IRONCREW_API_TOKEN") };

    let temp = tempfile::tempdir().unwrap();
    let flows_dir = temp.path().to_path_buf();

    // Two flow projects under the flows root.
    for (name, script) in [("askflow", ASK_FLOW), ("parkflow", PARK_FLOW)] {
        let dir = flows_dir.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("crew.lua"), script).unwrap();
    }

    let ironcrew_dir = flows_dir.join(".ironcrew");
    std::fs::create_dir_all(&ironcrew_dir).unwrap();
    let store = create_store(ironcrew_dir).await.unwrap();

    let _ = Box::leak(Box::new(temp));

    let state = Arc::new(AppState {
        flows_dir: flows_dir.clone(),
        active_runs: Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new())),
        active_conversations: Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new())),
        max_active_conversations: 100,
        max_active_runs: 100,
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

    // The ActiveRun entry lingers ~5s for SSE drain, but answering the
    // aborted run's question must not succeed once the entry is gone.
    // Immediately after abort the bridge may still hold the entry; poll
    // until the answer stops being deliverable.
    let mut last_status = None;
    for _ in 0..140 {
        let resp = client
            .post(format!("{}/flows/parkflow/answer/{}", base, run_id))
            .json(&serde_json::json!({ "question_id": question_id, "answer": "x" }))
            .send()
            .await
            .unwrap();
        last_status = Some(resp.status());
        if resp.status() == 404 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(
        last_status,
        Some(reqwest::StatusCode::NOT_FOUND),
        "answer endpoint kept accepting answers after abort"
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
