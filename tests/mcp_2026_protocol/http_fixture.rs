use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::{
    Router,
    body::Bytes,
    extract::State,
    http::{HeaderMap, Method, StatusCode},
    response::{IntoResponse, Response},
    routing::any,
};
use serde_json::{Value, json};
use tokio::{sync::oneshot, task::JoinHandle};

const PROTOCOL_VERSION: &str = "2026-07-28";

#[derive(Clone, Debug)]
pub(super) struct CapturedRequest {
    pub(super) http_method: Method,
    pub(super) headers: HashMap<String, String>,
    pub(super) body: Value,
}

#[derive(Clone)]
struct FixtureState {
    requests: Arc<Mutex<Vec<CapturedRequest>>>,
    legacy_only: bool,
}

pub(super) struct HttpFixture {
    pub(super) url: String,
    pub(super) requests: Arc<Mutex<Vec<CapturedRequest>>>,
    shutdown: oneshot::Sender<()>,
    task: JoinHandle<()>,
}

impl HttpFixture {
    pub(super) async fn spawn(legacy_only: bool) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind MCP fixture");
        let address = listener.local_addr().expect("fixture address");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let state = FixtureState {
            requests: Arc::clone(&requests),
            legacy_only,
        };
        let app = Router::new()
            .route("/mcp", any(handle_request))
            .with_state(state);
        let (shutdown, stopped) = oneshot::channel();
        let task = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = stopped.await;
                })
                .await
                .expect("serve MCP fixture");
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
        self.task.await.expect("join MCP fixture");
    }
}

async fn handle_request(
    State(state): State<FixtureState>,
    method: Method,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let body = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let captured_headers = headers
        .iter()
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|value| (name.as_str().to_owned(), value.to_owned()))
        })
        .collect();
    state
        .requests
        .lock()
        .expect("capture request")
        .push(CapturedRequest {
            http_method: method.clone(),
            headers: captured_headers,
            body: body.clone(),
        });

    if method != Method::POST {
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    }

    let id = body.get("id").cloned().unwrap_or(Value::Null);
    let rpc_result = match body.get("method").and_then(Value::as_str) {
        Some("server/discover") if state.legacy_only => json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {"code": -32601, "message": "server/discover is unavailable"}
        }),
        Some("server/discover") => rpc_result(
            id,
            json!({
                "resultType": "complete",
                "supportedVersions": [PROTOCOL_VERSION],
                "capabilities": {"tools": {}},
                "_meta": {
                    "io.modelcontextprotocol/serverInfo": {
                        "name": "ironcrew-http-fixture",
                        "version": "1.0.0"
                    }
                },
                "ttlMs": 60_000,
                "cacheScope": "private"
            }),
        ),
        Some("tools/list") => rpc_result(
            id,
            json!({
                "resultType": "complete",
                "tools": [{
                    "name": "echo",
                    "description": "Return the supplied text.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {"text": {"type": "string"}},
                        "required": ["text"],
                        "additionalProperties": false
                    }
                }],
                "ttlMs": 60_000,
                "cacheScope": "private"
            }),
        ),
        Some("tools/call") => {
            let text = body
                .pointer("/params/arguments/text")
                .and_then(Value::as_str)
                .unwrap_or_default();
            rpc_result(
                id,
                json!({
                    "resultType": "complete",
                    "content": [{"type": "text", "text": text}],
                    "isError": false
                }),
            )
        }
        other => json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {"code": -32601, "message": format!("unsupported method: {other:?}")}
        }),
    };

    axum::Json(rpc_result).into_response()
}

fn rpc_result(id: Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}
