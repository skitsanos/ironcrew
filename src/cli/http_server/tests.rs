use std::convert::Infallible;
use std::time::Duration;

use axum::body::Body;
use axum::http::StatusCode;
use axum::response::Response;
use axum::{Router, routing::get};
use bytes::Bytes;
use futures::StreamExt;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::serve;
use crate::cli::http_limits::HttpLimits;

async fn spawn(
    app: Router,
    limits: HttpLimits,
) -> (
    std::net::SocketAddr,
    tokio::sync::watch::Sender<bool>,
    tokio::task::JoinHandle<()>,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (shutdown, receiver) = tokio::sync::watch::channel(false);
    let task = tokio::spawn(async move {
        serve(listener, app, receiver, limits).await.unwrap();
    });
    (address, shutdown, task)
}

async fn read_response(stream: &mut tokio::net::TcpStream) -> String {
    let mut response = Vec::new();
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            let mut chunk = [0_u8; 1024];
            let count = stream.read(&mut chunk).await.unwrap();
            if count == 0 {
                break;
            }
            response.extend_from_slice(&chunk[..count]);
            if response.windows(4).any(|window| window == b"\r\n\r\n") && response.ends_with(b"ok")
            {
                break;
            }
        }
    })
    .await
    .expect("HTTP response deadline");
    String::from_utf8(response).unwrap()
}

fn assert_connection_closed(result: std::io::Result<usize>) {
    match result {
        Ok(0) => {}
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::ConnectionAborted | std::io::ErrorKind::ConnectionReset
            ) => {}
        other => panic!("expected a closed connection, got {other:?}"),
    }
}

#[tokio::test]
async fn preface_and_keep_alive_headers_have_deadlines() {
    let limits = HttpLimits::for_test(Duration::from_millis(50), Duration::from_secs(1), 4);
    let (address, shutdown, task) =
        spawn(Router::new().route("/", get(|| async { "ok" })), limits).await;

    let mut silent = tokio::net::TcpStream::connect(address).await.unwrap();
    let mut byte = [0_u8; 1];
    let result = tokio::time::timeout(Duration::from_secs(1), silent.read(&mut byte))
        .await
        .expect("silent preface must be closed");
    assert_connection_closed(result);

    let mut keep_alive = tokio::net::TcpStream::connect(address).await.unwrap();
    keep_alive
        .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .await
        .unwrap();
    assert!(
        read_response(&mut keep_alive)
            .await
            .starts_with("HTTP/1.1 200")
    );
    keep_alive.write_all(b"G").await.unwrap();
    let result = tokio::time::timeout(Duration::from_secs(1), keep_alive.read(&mut byte))
        .await
        .expect("second request headers must be closed");
    assert_connection_closed(result);

    shutdown.send_replace(true);
    task.await.unwrap();
}

#[tokio::test]
async fn connection_cap_waits_for_capacity_before_accepting() {
    let limits = HttpLimits::for_test(Duration::from_secs(5), Duration::from_secs(1), 1);
    let (address, shutdown, task) =
        spawn(Router::new().route("/", get(|| async { "ok" })), limits).await;
    let mut first = tokio::net::TcpStream::connect(address).await.unwrap();
    first.write_all(b"G").await.unwrap();
    tokio::time::sleep(Duration::from_millis(25)).await;

    let mut second = tokio::net::TcpStream::connect(address).await.unwrap();
    second
        .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut byte = [0_u8; 1];
    assert!(
        tokio::time::timeout(Duration::from_millis(100), second.read(&mut byte))
            .await
            .is_err()
    );
    drop(first);
    assert!(read_response(&mut second).await.starts_with("HTTP/1.1 200"));

    shutdown.send_replace(true);
    task.await.unwrap();
}

#[tokio::test]
async fn request_deadline_returns_408_but_does_not_wrap_stream_bodies() {
    let timeout = Duration::from_millis(25);
    let delayed = || async {
        tokio::time::sleep(Duration::from_millis(100)).await;
        "late"
    };
    let streaming = || async {
        let stream = async_stream::stream! {
            yield Ok::<Bytes, Infallible>(Bytes::from_static(b"first"));
            tokio::time::sleep(Duration::from_millis(100)).await;
            yield Ok::<Bytes, Infallible>(Bytes::from_static(b"second"));
        };
        Response::new(Body::from_stream(stream))
    };
    let app = Router::new()
        .route("/slow", get(delayed))
        .route("/stream", get(streaming));
    let limits = HttpLimits::for_test(Duration::from_secs(1), timeout, 4);
    let app = limits.apply(app);
    let (address, shutdown, task) = spawn(app, limits).await;

    let client = reqwest::Client::new();
    let timed_out = client
        .get(format!("http://{address}/slow"))
        .send()
        .await
        .unwrap();
    assert_eq!(timed_out.status(), StatusCode::REQUEST_TIMEOUT);
    assert_eq!(timed_out.headers()["cache-control"], "no-store");

    let response = client
        .get(format!("http://{address}/stream"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let mut chunks = response.bytes_stream();
    assert_eq!(chunks.next().await.unwrap().unwrap(), "first");
    assert_eq!(chunks.next().await.unwrap().unwrap(), "second");

    shutdown.send_replace(true);
    task.await.unwrap();
}
