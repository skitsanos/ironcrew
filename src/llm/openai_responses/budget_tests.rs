use super::*;
use crate::usage::budget::{BudgetState, TokenBudget};
use axum::{Json, Router, extract::State, response::IntoResponse, routing::post};
use std::sync::{Arc, Mutex};

#[derive(Clone)]
struct Fixture {
    mode: &'static str,
    calls: Arc<Mutex<Vec<(String, Value)>>>,
    entered: Arc<tokio::sync::Notify>,
}

async fn count(State(state): State<Fixture>, Json(body): Json<Value>) -> axum::response::Response {
    if state.mode == "tools" && body["tools"][0]["strict"] != false {
        return axum::http::StatusCode::BAD_REQUEST.into_response();
    }
    state.calls.lock().unwrap().push(("count".into(), body));
    match state.mode {
        "bad_count" => Json(json!({"input_tokens": -1})).into_response(),
        "oversized_count" => "x".repeat(4097).into_response(),
        "count_error" => axum::http::StatusCode::TOO_MANY_REQUESTS.into_response(),
        _ => Json(json!({"object":"response.input_tokens","input_tokens":10})).into_response(),
    }
}

async fn generate(
    State(state): State<Fixture>,
    Json(body): Json<Value>,
) -> axum::response::Response {
    state.calls.lock().unwrap().push(("generate".into(), body));
    state.entered.notify_one();
    if state.mode == "hang" {
        std::future::pending::<()>().await;
    }
    let input = if state.mode == "overrun" { 11 } else { 10 };
    let usage = if state.mode == "missing" {
        Value::Null
    } else {
        json!({"input_tokens":input,"output_tokens":3,"total_tokens":input+3})
    };
    if state.mode == "http_error" {
        return (
            axum::http::StatusCode::BAD_GATEWAY,
            Json(json!({"error":{"message":"fixture"},"usage":usage})),
        )
            .into_response();
    }
    if state.mode == "decode_error" {
        return Json(json!({"status":"completed","usage":usage})).into_response();
    }
    if state.mode == "stream" {
        return ([("content-type", "text/event-stream")], format!(
            "event: response.output_text.delta\ndata: {{\"delta\":\"ok\"}}\n\nevent: response.completed\ndata: {}\n\n",
            json!({"response":{"usage":usage}})
        )).into_response();
    }
    Json(json!({"status":"completed","usage":usage,"output":[{"type":"message","content":[{"type":"output_text","text":"ok"}]}]})).into_response()
}

async fn fixture(
    mode: &'static str,
    limit: u64,
) -> (
    OpenAiResponsesProvider,
    UsageTracker,
    Fixture,
    tokio::task::JoinHandle<()>,
) {
    let state = Fixture {
        mode,
        calls: Default::default(),
        entered: Default::default(),
    };
    let router = Router::new()
        .route("/v1/responses/input_tokens", post(count))
        .route("/v1/responses", post(generate))
        .with_state(state.clone());
    let socket = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", socket.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(socket, router).await.unwrap();
    });
    (
        OpenAiResponsesProvider::new("fixture".into(), Some(base), ResponsesConfig::default()),
        UsageTracker::with_budget(TokenBudget::new(limit).unwrap()),
        state,
        task,
    )
}

fn request(tracker: &UsageTracker) -> ChatRequest {
    let mut request = crate::engine::agent::Agent::default()
        .chat_request("fixture".into(), vec![ChatMessage::user("hello")]);
    request.max_tokens = Some(10);
    request.usage_tracker = Some(tracker.clone());
    request
}

#[test]
fn budget_transport_contract() {
    const NAME: &str = "llm::openai_responses::budget_tests::budget_transport_contract";
    if std::env::var("IRONCREW_BUDGET_TEST_CHILD").as_deref() != Ok(NAME) {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", NAME, "--nocapture"])
            .env("IRONCREW_BUDGET_TEST_CHILD", NAME)
            .env("IRONCREW_ALLOW_PRIVATE_IPS", "true")
            .env("NO_PROXY", "127.0.0.1,localhost")
            .env("no_proxy", "127.0.0.1,localhost")
            .env_remove("IRONCREW_MAX_RUN_TOKENS")
            .env_remove("IRONCREW_RATE_LIMIT_MS")
            .env_remove("OPENAI_API_KEY")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
        for mode in ["ok", "tools", "stream", "missing", "http_error", "decode_error", "overrun", "bad_count", "oversized_count", "count_error"] {
            let (provider, tracker, state, server) = fixture(mode, 100).await;
            let result = if mode == "stream" {
                let (tx, _rx) = tokio::sync::mpsc::channel(16);
                provider.chat_stream(request(&tracker), tx).await
            } else if mode == "tools" {
                use crate::tools::{Tool, ask_human::AskHumanTool};
                provider.chat_with_tools(request(&tracker), &[AskHumanTool.schema()]).await
            } else { provider.chat(request(&tracker)).await };
            let budget = tracker.snapshot().unwrap().budget;
            let count_failed = ["bad_count", "oversized_count", "count_error"].contains(&mode);
            assert_eq!(result.is_ok(), ["ok", "tools", "stream", "missing"].contains(&mode), "{mode}: {result:?}");
            assert_eq!(budget.charged, if count_failed { 0 } else if ["missing", "overrun"].contains(&mode) {20} else {13}, "{mode}");
            assert_eq!(budget.in_flight, 0);
            assert_eq!(state.calls.lock().unwrap().len(), if count_failed {1} else {2});
            if count_failed { assert_eq!(budget.state, BudgetState::CountingFailed); }
            if mode == "overrun" { assert_eq!(budget.state, BudgetState::BoundViolated); }
            if ["ok", "tools"].contains(&mode) {
                let calls = state.calls.lock().unwrap();
                assert_eq!(calls[0].1["input"], calls[1].1["input"]);
                assert_eq!(calls[0].1["tools"], calls[1].1["tools"]);
                if mode == "tools" {
                    assert_eq!(calls[0].1["tools"][0]["strict"], false);
                    assert_eq!(calls[1].1["tools"][0]["parameters"]["required"], json!(["question"]));
                }
                assert!(calls[0].1.get("max_output_tokens").is_none());
                assert_eq!(calls[1].1["max_output_tokens"], 10);
            }
            server.abort();
        }
        let (provider, tracker, state, server) = fixture("ok", 19).await;
        assert!(provider.chat(request(&tracker)).await.is_err());
        assert_eq!(state.calls.lock().unwrap().len(), 1);
        assert_eq!(tracker.budget().snapshot().state, BudgetState::Exhausted);
        assert_eq!(tracker.snapshot().unwrap().settled.requests(), 0);
        server.abort();

        let (provider, tracker, state, server) = fixture("hang", 20).await;
        {
            let call = provider.chat(request(&tracker));
            tokio::pin!(call);
            tokio::select! { _ = &mut call => panic!("expected pending call"), () = state.entered.notified() => {} }
            assert_eq!(tracker.budget().snapshot().reserved, 20);
        }
        assert_eq!(tracker.budget().snapshot().retained, 20);
        assert_eq!(tracker.snapshot().unwrap().settled.requests(), 1);
        server.abort();

        let (provider, tracker, state, server) = fixture("ok", 5000).await;
        let mut req = request(&tracker);
        req.max_tokens = None;
        provider.chat(req).await.unwrap();
        assert_eq!(state.calls.lock().unwrap()[1].1["max_output_tokens"], 4096);
        server.abort();
    });
}
