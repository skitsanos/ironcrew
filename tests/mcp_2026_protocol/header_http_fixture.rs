use std::collections::HashMap;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

use axum::{
    Router,
    body::Bytes,
    extract::State,
    http::{HeaderMap, Method, StatusCode, header},
    response::{IntoResponse, Response},
    routing::any,
};
use serde_json::{Value, json};
use tokio::{sync::oneshot, task::JoinHandle};

use super::header_mismatch_cases::HeaderMismatchCase;

const PROTOCOL_VERSION: &str = "2026-07-28";

#[derive(Clone, Debug)]
pub(super) struct HeaderRequest {
    pub(super) headers: HashMap<String, String>,
    header_counts: HashMap<String, usize>,
    pub(super) body: Value,
}

impl HeaderRequest {
    pub(super) fn header(&self, name: &str) -> &str {
        self.headers.get(name).map(String::as_str).unwrap()
    }

    pub(super) fn header_count(&self, name: &str) -> usize {
        self.header_counts.get(name).copied().unwrap_or_default()
    }
}

pub(super) fn methods(requests: &[HeaderRequest]) -> Vec<&str> {
    requests
        .iter()
        .map(|request| request.body["method"].as_str().unwrap())
        .collect()
}

#[derive(Clone)]
struct FixtureState {
    requests: Arc<Mutex<Vec<HeaderRequest>>>,
    tools: Value,
    list_sse: bool,
    mismatch: Option<HeaderMismatchCase>,
    list_count: Arc<AtomicUsize>,
    call_count: Arc<AtomicUsize>,
}

pub(super) struct HeaderHttpFixture {
    pub(super) url: String,
    pub(super) requests: Arc<Mutex<Vec<HeaderRequest>>>,
    shutdown: oneshot::Sender<()>,
    task: JoinHandle<()>,
}

impl HeaderHttpFixture {
    pub(super) async fn spawn(tools: Value, list_sse: bool) -> Self {
        Self::spawn_inner(tools, list_sse, None).await
    }

    pub(super) async fn spawn_header_mismatch(case: HeaderMismatchCase) -> Self {
        Self::spawn_inner(Value::Null, false, Some(case)).await
    }

    pub(super) async fn spawn_header_mismatch_sse(case: HeaderMismatchCase) -> Self {
        Self::spawn_inner(Value::Null, true, Some(case)).await
    }

    async fn spawn_inner(
        tools: Value,
        list_sse: bool,
        mismatch: Option<HeaderMismatchCase>,
    ) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind parameter-header fixture");
        let address = listener.local_addr().expect("fixture address");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let state = FixtureState {
            requests: Arc::clone(&requests),
            tools,
            list_sse,
            mismatch,
            list_count: Arc::new(AtomicUsize::new(0)),
            call_count: Arc::new(AtomicUsize::new(0)),
        };
        let app = Router::new().route("/mcp", any(handle)).with_state(state);
        let (shutdown, stopped) = oneshot::channel();
        let task = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = stopped.await;
                })
                .await
                .expect("serve parameter-header fixture");
        });
        Self {
            url: format!("http://{address}/mcp"),
            requests,
            shutdown,
            task,
        }
    }

    pub(super) async fn shutdown(self) {
        let _ = self.shutdown.send(());
        self.task.await.expect("join parameter-header fixture");
    }
}

async fn handle(
    State(state): State<FixtureState>,
    method: Method,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if method != Method::POST {
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    }
    let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let captured_headers = headers
        .iter()
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|value| (name.as_str().to_owned(), value.to_owned()))
        })
        .collect();
    let header_counts = headers
        .keys()
        .map(|name| {
            (
                name.as_str().to_owned(),
                headers.get_all(name).iter().count(),
            )
        })
        .collect();
    state.requests.lock().unwrap().push(HeaderRequest {
        headers: captured_headers,
        header_counts,
        body: body.clone(),
    });
    let id = body.get("id").cloned().unwrap_or(Value::Null);
    match body.get("method").and_then(Value::as_str) {
        Some("server/discover") => json_response(rpc_result(
            id,
            json!({
                "resultType": "complete",
                "supportedVersions": [PROTOCOL_VERSION],
                "capabilities": {"tools": {}},
                "_meta": {"io.modelcontextprotocol/serverInfo": {
                    "name": "ironcrew-header-fixture", "version": "1.0.0"
                }},
                "ttlMs": 60_000,
                "cacheScope": "private"
            }),
        )),
        Some("tools/list") => {
            let list_index = state.list_count.fetch_add(1, Ordering::AcqRel);
            let cursor = body.pointer("/params/cursor").and_then(Value::as_str);
            let (tools, next_cursor) = if let Some(case) = state.mismatch {
                let page = case.page(list_index, cursor);
                (page.tools, page.next_cursor)
            } else {
                (state.tools.clone(), None)
            };
            let mut result = json!({
                "resultType": "complete",
                "tools": tools,
                "ttlMs": 60_000,
                "cacheScope": "private"
            });
            if let Some(next_cursor) = next_cursor {
                result["nextCursor"] = Value::String(next_cursor.to_owned());
            }
            let message = rpc_result(id, result);
            if state.list_sse {
                sse_response(message)
            } else {
                json_response(message)
            }
        }
        Some("tools/call") => {
            let call_index = state.call_count.fetch_add(1, Ordering::AcqRel);
            if let Some(case) = state.mismatch.filter(|case| case.rejects_call(call_index)) {
                mismatch_response(id, case.error_code())
            } else {
                json_response(rpc_result(
                    id,
                    json!({
                        "resultType": "complete",
                        "content": [{"type": "text", "text": "header-ok"}],
                        "isError": false
                    }),
                ))
            }
        }
        _ => StatusCode::BAD_REQUEST.into_response(),
    }
}

fn rpc_result(id: Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn json_response(value: Value) -> Response {
    axum::Json(value).into_response()
}

fn sse_response(value: Value) -> Response {
    let body = format!("data: {}\n\n", serde_json::to_string(&value).unwrap());
    ([(header::CONTENT_TYPE, "text/event-stream")], body).into_response()
}

fn mismatch_response(id: Value, code: i64) -> Response {
    (
        StatusCode::BAD_REQUEST,
        axum::Json(json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {
                "code": code,
                "message": "Header mismatch: fixture requested one schema refresh"
            }
        })),
    )
        .into_response()
}
