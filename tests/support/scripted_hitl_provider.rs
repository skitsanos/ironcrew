use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{Value, json};

pub const API_KEY: &str = "hitl-agent-local-mock-key";
pub const ANALYST_PROMPT: &str = "[analyst] Which dataset should I analyze?";
pub const REVIEWER_PROMPT: &str = "[reviewer] Should I approve the analysis?";
pub const ANALYST_ANSWER: &str = "dataset-alpha";
pub const REVIEWER_ANSWER: &str = "approved";

const MAX_REQUESTS: usize = 8;
const ANALYST_MARKER: &str = "ANALYST_HITL_CHECKPOINT";
const REVIEWER_MARKER: &str = "REVIEWER_HITL_CHECKPOINT";

struct ProviderState {
    requests: AtomicUsize,
}

pub struct ScriptedHitlProvider {
    state: Arc<ProviderState>,
    pub base_url: String,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

#[derive(Clone)]
pub struct ScriptedHitlProbe {
    state: Arc<ProviderState>,
    pub base_url: String,
}

impl ScriptedHitlProvider {
    pub fn start() -> Self {
        let state = Arc::new(ProviderState {
            requests: AtomicUsize::new(0),
        });
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
        let server_state = state.clone();
        let thread = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .expect("build HITL mock-provider runtime");
            runtime.block_on(async move {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                    .await
                    .expect("bind HITL mock provider");
                let address = listener
                    .local_addr()
                    .expect("read HITL mock-provider address");
                ready_tx
                    .send(format!("http://{address}/v1"))
                    .expect("publish HITL mock-provider URL");
                let app = Router::new()
                    .route("/v1/chat/completions", post(complete))
                    .with_state(server_state);
                axum::serve(listener, app)
                    .with_graceful_shutdown(async move {
                        let _ = shutdown_rx.await;
                    })
                    .await
                    .expect("serve HITL mock provider");
            });
        });
        let base_url = ready_rx.recv().expect("receive HITL mock-provider URL");
        Self {
            state,
            base_url,
            shutdown: Some(shutdown_tx),
            thread: Some(thread),
        }
    }

    pub fn probe(&self) -> ScriptedHitlProbe {
        ScriptedHitlProbe {
            state: self.state.clone(),
            base_url: self.base_url.clone(),
        }
    }
}

impl ScriptedHitlProbe {
    pub fn assert_complete(&self) {
        assert_eq!(
            self.state.requests.load(Ordering::SeqCst),
            4,
            "two agents must each ask once and consume one answer"
        );
    }
}

impl Drop for ScriptedHitlProvider {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(thread) = self.thread.take() {
            thread.join().expect("join HITL mock-provider thread");
        }
    }
}

async fn complete(
    State(state): State<Arc<ProviderState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    if headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        != Some(&format!("Bearer {API_KEY}"))
    {
        return error(StatusCode::UNAUTHORIZED, "unauthorized");
    }
    if !body["tools"].as_array().is_some_and(|tools| {
        tools
            .iter()
            .any(|tool| tool["function"]["name"] == "ask_human")
    }) {
        return error(StatusCode::BAD_REQUEST, "ask_human tool missing");
    }

    let sequence = state.requests.fetch_add(1, Ordering::SeqCst) + 1;
    if sequence > MAX_REQUESTS {
        return error(StatusCode::TOO_MANY_REQUESTS, "request bound exceeded");
    }
    if let Some(answer) = last_tool_result(&body) {
        return completion(sequence, format!("FINAL:{answer}"));
    }

    let serialized = body["messages"].to_string();
    let question = if serialized.contains(ANALYST_MARKER) {
        "Which dataset should I analyze?"
    } else if serialized.contains(REVIEWER_MARKER) {
        "Should I approve the analysis?"
    } else {
        return error(StatusCode::BAD_REQUEST, "unknown HITL task marker");
    };
    (
        StatusCode::OK,
        Json(json!({
            "id": format!("hitl-{sequence}"),
            "object": "chat.completion",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": format!("hitl-call-{sequence}"),
                        "type": "function",
                        "function": {
                            "name": "ask_human",
                            "arguments": serde_json::to_string(&json!({
                                "question": question,
                                "timeout_s": 30,
                            })).expect("serialize HITL tool arguments"),
                        }
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": { "prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2 }
        })),
    )
        .into_response()
}

fn last_tool_result(body: &Value) -> Option<&str> {
    body["messages"]
        .as_array()?
        .iter()
        .rev()
        .find(|message| message["role"] == "tool")?["content"]
        .as_str()
}

fn completion(sequence: usize, content: String) -> Response {
    (
        StatusCode::OK,
        Json(json!({
            "id": format!("hitl-{sequence}"),
            "object": "chat.completion",
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": content },
                "finish_reason": "stop"
            }],
            "usage": { "prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2 }
        })),
    )
        .into_response()
}

fn error(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({ "error": { "message": message } }))).into_response()
}
