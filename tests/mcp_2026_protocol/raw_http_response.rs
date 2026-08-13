use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;

const INBOUND_LIMIT: usize = 1024 * 1024;

pub(super) fn basic_tool_list(id: Value) -> Value {
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
    })
}

pub(super) async fn write_json(stream: &mut TcpStream, value: Value, close: bool) {
    let body = serde_json::to_vec(&value).expect("serialize raw fixture response");
    let connection = if close { "close" } else { "keep-alive" };
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: {connection}\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(head.as_bytes()).await;
    let _ = stream.write_all(&body).await;
    let _ = stream.flush().await;
}

pub(super) async fn write_fixed(stream: &mut TcpStream, content_type: &str, body: &[u8]) {
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(head.as_bytes()).await;
    let _ = stream.write_all(body).await;
    let _ = stream.flush().await;
}

pub(super) async fn write_empty_status(stream: &mut TcpStream, status: u16, reason: &str) {
    let head =
        format!("HTTP/1.1 {status} {reason}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
    let _ = stream.write_all(head.as_bytes()).await;
    let _ = stream.flush().await;
}

pub(super) async fn write_mixed_sse(stream: &mut TcpStream, value: Value, ending: &[u8]) {
    let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
                Transfer-Encoding: chunked\r\nConnection: close\r\n\r\n";
    if stream.write_all(head.as_bytes()).await.is_err() {
        return;
    }
    let prefix = format!("data: {}", serde_json::to_string(&value).unwrap());
    if !write_chunk(stream, prefix.as_bytes()).await {
        return;
    }
    for byte in ending {
        if !write_chunk(stream, std::slice::from_ref(byte)).await {
            return;
        }
    }
    let _ = stream.write_all(b"0\r\n\r\n").await;
    let _ = stream.flush().await;
}

pub(super) async fn write_keepalive_sse(stream: &mut TcpStream, value: Value) {
    let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
                Transfer-Encoding: chunked\r\nConnection: close\r\n\r\n";
    if stream.write_all(head.as_bytes()).await.is_err() {
        return;
    }
    let data = format!("data: {}", serde_json::to_string(&value).unwrap());
    for chunk in [
        b": keep".as_slice(),
        b"alive-one\r".as_slice(),
        b"\n: keepalive".as_slice(),
        b"-two\n\n".as_slice(),
        b"da".as_slice(),
        data.as_bytes().strip_prefix(b"da").unwrap(),
        b"\r".as_slice(),
        b"\n\r\n".as_slice(),
    ] {
        if !write_chunk(stream, chunk).await {
            return;
        }
    }
    let _ = stream.write_all(b"0\r\n\r\n").await;
    let _ = stream.flush().await;
}

pub(super) async fn write_oversized(stream: &mut TcpStream, sse: bool, id: Value) {
    let content_type = if sse {
        "text/event-stream"
    } else {
        "application/json"
    };
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n"
    );
    if stream.write_all(head.as_bytes()).await.is_err() {
        return;
    }
    let data = if sse { "data: " } else { "" };
    let prefix = format!(
        "{data}{{\"jsonrpc\":\"2.0\",\"id\":{id},\"result\":{{\"resultType\":\"complete\",\"content\":[{{\"type\":\"text\",\"text\":\""
    );
    write_chunk(stream, prefix.as_bytes()).await;
    let chunk = vec![b'x'; 16 * 1024];
    for _ in 0..=(INBOUND_LIMIT / chunk.len()) {
        if !write_chunk(stream, &chunk).await {
            return;
        }
    }
}

async fn write_chunk(stream: &mut TcpStream, bytes: &[u8]) -> bool {
    let head = format!("{:x}\r\n", bytes.len());
    stream.write_all(head.as_bytes()).await.is_ok()
        && stream.write_all(bytes).await.is_ok()
        && stream.write_all(b"\r\n").await.is_ok()
        && stream.flush().await.is_ok()
}
