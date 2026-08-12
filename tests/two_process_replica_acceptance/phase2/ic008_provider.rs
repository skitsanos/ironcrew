use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{Value, json};
use tokio::sync::Notify;

const AUTHORIZATION: &str = "Bearer ic008-local-mock-key";
const MAX_REQUESTS: usize = 16;

struct ProviderState {
    requests: AtomicUsize,
    blocked: AtomicBool,
    blocking_content: String,
    release: Notify,
}

#[derive(Clone)]
pub(super) struct ProviderProbe {
    state: Arc<ProviderState>,
    pub(super) base_url: String,
}

pub(super) struct MockProvider {
    probe: ProviderProbe,
    task: tokio::task::JoinHandle<()>,
}

impl MockProvider {
    pub(super) async fn start(blocking_content: &str) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind IC-008 mock provider");
        let address = listener
            .local_addr()
            .expect("read IC-008 mock provider address");
        let state = Arc::new(ProviderState {
            requests: AtomicUsize::new(0),
            blocked: AtomicBool::new(false),
            blocking_content: blocking_content.to_owned(),
            release: Notify::new(),
        });
        let app = Router::new()
            .route("/v1/chat/completions", post(complete))
            .with_state(state.clone());
        let task = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("serve IC-008 mock provider");
        });
        Self {
            probe: ProviderProbe {
                state,
                base_url: format!("http://{address}/v1"),
            },
            task,
        }
    }

    pub(super) fn probe(&self) -> ProviderProbe {
        self.probe.clone()
    }
}

impl Drop for MockProvider {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl ProviderProbe {
    pub(super) fn request_count(&self) -> usize {
        self.state.requests.load(Ordering::SeqCst)
    }

    pub(super) async fn assert_stable_count(&self, expected: usize) {
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            self.request_count(),
            expected,
            "IC-008 provider request count"
        );
    }

    pub(super) async fn wait_until_blocked(&self, expected_count: usize) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if self.request_count() == expected_count && self.state.blocked.load(Ordering::SeqCst) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!(
            "IC-008 provider did not block request {expected_count}; observed {}",
            self.request_count()
        );
    }

    pub(super) fn release_blocked(&self) {
        self.state.release.notify_one();
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
        != Some(AUTHORIZATION)
    {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "unauthorized" })),
        )
            .into_response();
    }
    let Some(content) = last_user_content(&body) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "missing user content" })),
        )
            .into_response();
    };
    let sequence = state.requests.fetch_add(1, Ordering::SeqCst) + 1;
    if sequence > MAX_REQUESTS {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            Json(json!({ "error": "mock request bound exceeded" })),
        )
            .into_response();
    }
    if content == state.blocking_content {
        state.blocked.store(true, Ordering::SeqCst);
        state.release.notified().await;
        state.blocked.store(false, Ordering::SeqCst);
    }
    (
        StatusCode::OK,
        Json(json!({
            "id": format!("ic008-{sequence}"),
            "object": "chat.completion",
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": format!("mock:{content}") },
                "finish_reason": "stop"
            }],
            "usage": { "prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2 }
        })),
    )
        .into_response()
}

fn last_user_content(body: &Value) -> Option<String> {
    body["messages"]
        .as_array()?
        .iter()
        .rev()
        .find(|message| message["role"] == "user")?["content"]
        .as_str()
        .map(str::to_owned)
}
