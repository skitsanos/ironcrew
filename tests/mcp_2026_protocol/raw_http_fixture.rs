use std::collections::HashMap;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;
use tokio::task::{JoinHandle, JoinSet};

const PROTOCOL_VERSION: &str = "2026-07-28";
use super::raw_http_response::{
    basic_tool_list, write_empty_status, write_fixed, write_json, write_keepalive_sse,
    write_mixed_sse, write_oversized,
};

#[derive(Clone, Copy, Debug)]
pub(super) enum RawBehavior {
    Accepted202,
    HangBeforeHeaders,
    HangDiscovery,
    KeepaliveThenDataSse,
    MixedEndingSse,
    NoContent204,
    OversizedChunkedJson,
    OversizedUnterminatedSse,
    ServerRequest,
    SseIdThenClose,
    TaskNotification,
}

#[derive(Clone, Debug)]
pub(super) struct RawRequest {
    pub(super) http_method: String,
    pub(super) headers: HashMap<String, String>,
    pub(super) body: Value,
}

pub(super) struct RawHttpFixture {
    pub(super) url: String,
    pub(super) requests: Arc<Mutex<Vec<RawRequest>>>,
    pub(super) hang_closed: Arc<AtomicBool>,
    pub(super) bytes_after_hang: Arc<AtomicUsize>,
    shutdown: oneshot::Sender<()>,
    task: JoinHandle<()>,
}

impl RawHttpFixture {
    pub(super) async fn spawn(behavior: RawBehavior) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind raw HTTP fixture");
        let address = listener.local_addr().expect("raw fixture address");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let hang_closed = Arc::new(AtomicBool::new(false));
        let bytes_after_hang = Arc::new(AtomicUsize::new(0));
        let (shutdown, mut stopped) = oneshot::channel();
        let task_requests = Arc::clone(&requests);
        let task_hang_closed = Arc::clone(&hang_closed);
        let task_bytes_after_hang = Arc::clone(&bytes_after_hang);
        let task = tokio::spawn(async move {
            let mut connections = JoinSet::new();
            loop {
                tokio::select! {
                    _ = &mut stopped => break,
                    accepted = listener.accept() => {
                        let (stream, _) = accepted.expect("accept raw HTTP fixture connection");
                        connections.spawn(handle_connection(
                            stream,
                            behavior,
                            Arc::clone(&task_requests),
                            Arc::clone(&task_hang_closed),
                            Arc::clone(&task_bytes_after_hang),
                        ));
                    }
                    Some(result) = connections.join_next(), if !connections.is_empty() => {
                        result.expect("raw HTTP fixture connection task");
                    }
                }
            }
            connections.abort_all();
            while connections.join_next().await.is_some() {}
        });
        Self {
            url: format!("http://{address}/mcp"),
            requests,
            hang_closed,
            bytes_after_hang,
            shutdown,
            task,
        }
    }

    pub(super) async fn shutdown(self) {
        let _ = self.shutdown.send(());
        self.task.await.expect("join raw HTTP fixture");
    }
}

async fn handle_connection(
    stream: TcpStream,
    behavior: RawBehavior,
    requests: Arc<Mutex<Vec<RawRequest>>>,
    hang_closed: Arc<AtomicBool>,
    bytes_after_hang: Arc<AtomicUsize>,
) {
    let mut stream = BufReader::new(stream);
    loop {
        let Some(request) = read_request(&mut stream).await else {
            return;
        };
        let rpc_method = request
            .body
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let is_discovery = rpc_method == "server/discover";
        let id = request.body.get("id").cloned().unwrap_or(Value::Null);
        let request_count = {
            let mut requests = requests.lock().expect("capture raw request");
            requests.push(request);
            requests.len()
        };
        if is_discovery && !matches!(behavior, RawBehavior::HangDiscovery) {
            write_json(stream.get_mut(), discover_response(id), false).await;
            continue;
        }
        match behavior {
            RawBehavior::Accepted202 => write_empty_status(stream.get_mut(), 202, "Accepted").await,
            RawBehavior::HangBeforeHeaders if rpc_method == "tools/list" && request_count == 2 => {
                write_json(stream.get_mut(), basic_tool_list(id), false).await;
                continue;
            }
            RawBehavior::HangBeforeHeaders | RawBehavior::HangDiscovery => {
                let mut byte = [0_u8; 1];
                loop {
                    match stream.read(&mut byte).await {
                        Ok(0) | Err(_) => {
                            hang_closed.store(true, Ordering::Release);
                            return;
                        }
                        Ok(count) => {
                            bytes_after_hang.fetch_add(count, Ordering::AcqRel);
                        }
                    }
                }
            }
            RawBehavior::OversizedChunkedJson => {
                write_oversized(stream.get_mut(), false, id).await;
            }
            RawBehavior::OversizedUnterminatedSse => {
                write_oversized(stream.get_mut(), true, id).await;
            }
            RawBehavior::MixedEndingSse => {
                let result = if rpc_method == "tools/list" {
                    json!({
                        "resultType": "complete",
                        "tools": [{
                            "name": "echo",
                            "description": "Return supplied text.",
                            "inputSchema": {"type": "object"}
                        }],
                        "ttlMs": 60_000,
                        "cacheScope": "private"
                    })
                } else {
                    json!({
                        "resultType": "complete",
                        "content": [{"type": "text", "text": "mixed-ok"}],
                        "isError": false
                    })
                };
                let ending = if rpc_method == "tools/list" {
                    b"\n\r\n".as_slice()
                } else {
                    b"\r\n\n".as_slice()
                };
                write_mixed_sse(
                    stream.get_mut(),
                    json!({
                        "jsonrpc": "2.0", "id": id, "result": result
                    }),
                    ending,
                )
                .await;
            }
            RawBehavior::KeepaliveThenDataSse => {
                write_keepalive_sse(
                    stream.get_mut(),
                    json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "resultType": "complete",
                            "tools": [{
                                "name": "echo",
                                "description": "Return supplied text.",
                                "inputSchema": {"type": "object"}
                            }],
                            "ttlMs": 60_000,
                            "cacheScope": "private"
                        }
                    }),
                )
                .await;
            }
            RawBehavior::NoContent204 => {
                write_empty_status(stream.get_mut(), 204, "No Content").await
            }
            RawBehavior::ServerRequest => {
                write_json(
                    stream.get_mut(),
                    json!({"jsonrpc": "2.0", "id": "server-1", "method": "ping", "params": {}}),
                    true,
                )
                .await;
            }
            RawBehavior::SseIdThenClose => {
                write_fixed(stream.get_mut(), "text/event-stream", b"id: event-1\n").await;
            }
            RawBehavior::TaskNotification => {
                write_json(
                    stream.get_mut(),
                    json!({
                        "jsonrpc": "2.0",
                        "method": "notifications/tasks",
                        "params": {
                            "taskId": "fixture-task",
                            "status": "working",
                            "createdAt": "2026-08-13T00:00:00Z",
                            "lastUpdatedAt": "2026-08-13T00:00:00Z",
                            "ttlMs": 60_000
                        }
                    }),
                    true,
                )
                .await;
            }
        }
        return;
    }
}

async fn read_request(stream: &mut BufReader<TcpStream>) -> Option<RawRequest> {
    let mut request_line = String::new();
    if stream.read_line(&mut request_line).await.ok()? == 0 {
        return None;
    }
    let http_method = request_line.split_whitespace().next()?.to_owned();
    let mut headers = HashMap::new();
    loop {
        let mut line = String::new();
        if stream.read_line(&mut line).await.ok()? == 0 {
            return None;
        }
        if line == "\r\n" || line == "\n" {
            break;
        }
        let (name, value) = line.split_once(':')?;
        headers.insert(name.to_ascii_lowercase(), value.trim().to_owned());
    }
    let length = headers
        .get("content-length")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or_default();
    let mut body = vec![0_u8; length];
    stream.read_exact(&mut body).await.ok()?;
    Some(RawRequest {
        http_method,
        headers,
        body: serde_json::from_slice(&body).unwrap_or(Value::Null),
    })
}

fn discover_response(id: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {
            "resultType": "complete",
            "supportedVersions": [PROTOCOL_VERSION],
            "capabilities": {"tools": {}},
            "_meta": {"io.modelcontextprotocol/serverInfo": {
                "name": "ironcrew-raw-http-fixture", "version": "1.0.0"
            }},
            "ttlMs": 60_000,
            "cacheScope": "private"
        }
    })
}
