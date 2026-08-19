use super::redirect_policy::carries_credentials;
use super::*;
use crate::tools::{Tool, ToolCallContext};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Minimal one-shot HTTP server. `respond` receives the raw request head
/// and returns the raw response to write back.
async fn serve_once(
    listener: TcpListener,
    respond: impl Fn(String) -> String + Send + 'static,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        if let Ok((mut stream, _)) = listener.accept().await {
            let mut buffer = vec![0u8; 8192];
            let read = stream.read(&mut buffer).await.unwrap_or(0);
            let head = String::from_utf8_lossy(&buffer[..read]).into_owned();
            let response = respond(head);
            let _ = stream.write_all(response.as_bytes()).await;
            let _ = stream.flush().await;
        }
    })
}

#[tokio::test]
async fn cross_origin_redirect_does_not_forward_the_api_key_header() {
    // Destination: records whether it ever saw the secret header.
    let destination = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let destination_port = destination.local_addr().unwrap().port();
    let leaked = Arc::new(AtomicBool::new(false));
    let observed = leaked.clone();
    let destination_task = serve_once(destination, move |head| {
        if head.to_ascii_lowercase().contains("x-api-key") {
            observed.store(true, Ordering::SeqCst);
        }
        "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok".to_string()
    })
    .await;

    // Origin: redirects to the destination, a different origin.
    let origin = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin_port = origin.local_addr().unwrap().port();
    let origin_task = serve_once(origin, move |_| {
        format!(
            "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:{destination_port}/\r\nContent-Length: 0\r\n\r\n"
        )
    })
    .await;

    // allow_private = true so loopback is reachable in the test.
    let result = HttpRequestTool::with_policy_for_test(64 * 1024, true)
        .execute(
            serde_json::json!({
                "url": format!("http://127.0.0.1:{origin_port}/"),
                "method": "GET",
                "auth_type": "api_key",
                "auth_token": "secret-canary",
            }),
            &ToolCallContext::default(),
        )
        .await;

    origin_task.abort();
    destination_task.abort();

    assert!(
        !leaked.load(Ordering::SeqCst),
        "the API key header was forwarded across a cross-origin redirect"
    );
    // reqwest wraps the policy's message, so assert on the redirect failure
    // itself; the leak assertion above is the real contract.
    let error = result.expect_err("a cross-origin redirect must not be followed");
    assert!(
        error.to_string().contains("redirect"),
        "unexpected error: {error}"
    );
}

#[tokio::test]
async fn same_origin_redirect_is_still_followed_with_credentials() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();

    // Serve two requests on one origin: redirect, then the final response.
    let server = tokio::spawn(async move {
        for hop in 0..2 {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            let mut buffer = vec![0u8; 8192];
            let _ = stream.read(&mut buffer).await;
            let response = if hop == 0 {
                format!(
                    "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:{port}/final\r\nContent-Length: 0\r\n\r\n"
                )
            } else {
                "HTTP/1.1 200 OK\r\nContent-Length: 7\r\n\r\narrived".to_string()
            };
            let _ = stream.write_all(response.as_bytes()).await;
            let _ = stream.flush().await;
        }
    });

    let result = HttpRequestTool::with_policy_for_test(64 * 1024, true)
        .execute(
            serde_json::json!({
                "url": format!("http://127.0.0.1:{port}/"),
                "method": "GET",
                "auth_type": "api_key",
                "auth_token": "secret-canary",
            }),
            &ToolCallContext::default(),
        )
        .await;
    server.abort();

    let body = result.expect("a same-origin redirect must still be followed");
    assert!(body.contains("arrived"), "unexpected response: {body}");
}

#[test]
fn credential_detection_covers_auth_and_custom_headers() {
    assert!(carries_credentials(&serde_json::json!({
        "auth_type": "bearer", "auth_token": "t"
    })));
    assert!(carries_credentials(&serde_json::json!({
        "headers": {"x-tenant": "acme"}
    })));
    assert!(!carries_credentials(&serde_json::json!({
        "url": "https://example.com", "method": "GET"
    })));
    assert!(!carries_credentials(&serde_json::json!({
        "auth_type": "none", "headers": {}
    })));
}
