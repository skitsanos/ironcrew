use std::collections::HashMap;
use std::sync::{Arc, atomic::Ordering};
use std::time::Duration;

use ironcrew::mcp::{
    client::McpClient,
    config::{McpServerConfig, McpTransportConfig},
};
use serde_json::json;

use super::boundary_test_support::isolate_environment;
use super::http_fixture::HttpFixture;
use super::raw_http_fixture::{RawBehavior, RawHttpFixture};

fn config(url: &str) -> McpServerConfig {
    McpServerConfig {
        label: "boundary-fixture".into(),
        transport: McpTransportConfig::Http {
            url: url.into(),
            headers: HashMap::new(),
        },
        execution_identity_fingerprint: Some("boundary-v1".into()),
        inherit_env: false,
    }
}

#[tokio::test]
async fn rejects_oversized_chunked_json_before_materialization() {
    assert_violation(RawBehavior::OversizedChunkedJson).await;
}

#[tokio::test]
async fn rejects_oversized_unterminated_sse_before_materialization() {
    assert_violation(RawBehavior::OversizedUnterminatedSse).await;
}

#[tokio::test]
async fn request_only_http_rejects_202_and_204_and_poisons() {
    assert_violation(RawBehavior::Accepted202).await;
    assert_violation(RawBehavior::NoContent204).await;
}

#[tokio::test]
async fn mixed_whatwg_sse_endings_decode_across_transport_chunks() {
    isolate_environment();
    let fixture = RawHttpFixture::spawn(RawBehavior::MixedEndingSse).await;
    let client = McpClient::connect(&config(&fixture.url))
        .await
        .expect("connect fixture");
    let tools = client.list_all_tools().await.expect("LF plus CRLF event");
    assert_eq!(tools.len(), 1);
    let result = client
        .call_tool("echo", json!({}))
        .await
        .expect("CRLF plus LF event");
    let text = result.content[0].as_text().expect("text content");
    assert_eq!(text.text, "mixed-ok");
    client.shutdown().await;
    assert_eq!(fixture.requests.lock().unwrap().len(), 3);
    fixture.shutdown().await;
}

#[tokio::test]
async fn split_repeated_sse_keepalives_are_ignored_before_valid_data() {
    isolate_environment();
    let fixture = RawHttpFixture::spawn(RawBehavior::KeepaliveThenDataSse).await;
    let client = McpClient::connect(&config(&fixture.url)).await.unwrap();
    let tools = client
        .list_all_tools()
        .await
        .expect("split comment-only events must not hide following data");
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, "echo");
    client.shutdown().await;
    assert_eq!(fixture.requests.lock().unwrap().len(), 2);
    fixture.shutdown().await;
}

#[tokio::test]
async fn server_request_poison_sends_no_response_or_second_call() {
    assert_violation(RawBehavior::ServerRequest).await;
}

#[tokio::test]
async fn task_notification_poison_sends_no_post_or_second_call() {
    assert_violation(RawBehavior::TaskNotification).await;
}

#[tokio::test]
async fn sse_id_then_close_does_not_resume_or_send_last_event_id() {
    isolate_environment();
    let fixture = RawHttpFixture::spawn(RawBehavior::SseIdThenClose).await;
    let client = McpClient::connect(&config(&fixture.url))
        .await
        .expect("connect fixture");
    client
        .list_all_tools()
        .await
        .expect_err("premature SSE close must fail");
    let _ = client.call_tool("echo", json!({})).await;
    client.shutdown().await;
    let requests = fixture.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 2, "no resume GET or later POST is allowed");
    assert!(requests.iter().all(|request| request.http_method == "POST"));
    assert!(
        requests
            .iter()
            .all(|request| !request.headers.contains_key("last-event-id"))
    );
    fixture.shutdown().await;
}

#[tokio::test]
async fn abort_before_headers_closes_socket_and_blocks_later_wire() {
    isolate_environment();
    let fixture = RawHttpFixture::spawn(RawBehavior::HangBeforeHeaders).await;
    let client = Arc::new(
        McpClient::connect(&config(&fixture.url))
            .await
            .expect("connect fixture"),
    );
    client.list_all_tools().await.expect("commit initial plan");
    let active = Arc::clone(&client);
    let list = tokio::spawn(async move { active.list_all_tools().await });
    wait_for_requests(&fixture, 3).await;
    list.abort();
    let _ = list.await;
    wait_for_close(&fixture).await;
    let _ = client.call_tool("echo", json!({})).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(fixture.requests.lock().unwrap().len(), 3);
    assert_eq!(fixture.bytes_after_hang.load(Ordering::Acquire), 0);
    client.shutdown().await;
    fixture.shutdown().await;
}

#[tokio::test]
async fn configured_list_timeout_closes_socket_and_blocks_later_wire() {
    isolate_environment();
    let fixture = RawHttpFixture::spawn(RawBehavior::HangBeforeHeaders).await;
    let client = McpClient::connect(&config(&fixture.url))
        .await
        .expect("connect fixture");
    client.list_all_tools().await.expect("commit initial plan");
    let error = client.list_all_tools().await.unwrap_err().to_string();
    assert!(
        error.contains("configured deadline"),
        "unexpected timeout: {error}"
    );
    wait_for_close(&fixture).await;
    let _ = client.call_tool("echo", json!({})).await;
    assert_eq!(fixture.requests.lock().unwrap().len(), 3);
    assert_eq!(fixture.bytes_after_hang.load(Ordering::Acquire), 0);
    client.shutdown().await;
    fixture.shutdown().await;
}

#[tokio::test]
async fn configured_call_timeout_closes_socket_and_blocks_later_wire() {
    isolate_environment();
    let fixture = RawHttpFixture::spawn(RawBehavior::HangBeforeHeaders).await;
    let client = McpClient::connect(&config(&fixture.url))
        .await
        .expect("connect fixture");
    client.list_all_tools().await.expect("commit initial plan");
    let error = client
        .call_tool("echo", json!({}))
        .await
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("configured deadline"),
        "unexpected timeout: {error}"
    );
    wait_for_close(&fixture).await;
    let _ = client.call_tool("echo", json!({})).await;
    assert_eq!(fixture.requests.lock().unwrap().len(), 3);
    assert_eq!(fixture.bytes_after_hang.load(Ordering::Acquire), 0);
    client.shutdown().await;
    fixture.shutdown().await;
}

#[tokio::test]
async fn discovery_timeout_and_caller_abort_close_exactly_one_post() {
    isolate_environment();
    for abort in [false, true] {
        let fixture = RawHttpFixture::spawn(RawBehavior::HangDiscovery).await;
        let url = fixture.url.clone();
        if abort {
            let connect = tokio::spawn(async move { McpClient::connect(&config(&url)).await });
            wait_for_requests(&fixture, 1).await;
            connect.abort();
            let _ = connect.await;
        } else {
            let error = match McpClient::connect(&config(&fixture.url)).await {
                Ok(client) => {
                    client.shutdown().await;
                    panic!("hanging discovery must time out")
                }
                Err(error) => error.to_string(),
            };
            assert!(error.contains("discovery timed out"));
        }
        wait_for_close(&fixture).await;
        let requests = fixture.requests.lock().unwrap().clone();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].body["method"], "server/discover");
        fixture.shutdown().await;
    }
}

#[tokio::test]
async fn configured_reserved_headers_fail_before_wire() {
    isolate_environment();
    for name in [
        "Accept",
        "Content-Type",
        "Content-Length",
        "Transfer-Encoding",
        "Host",
        "MCP-Session-Id",
        "Last-Event-ID",
        "MCP-Protocol-Version",
        "MCP-Method",
        "MCP-Name",
        "MCP-Param-Fixture",
    ] {
        let fixture = HttpFixture::spawn(false).await;
        let mut config = config(&fixture.url);
        let McpTransportConfig::Http { headers, .. } = &mut config.transport else {
            unreachable!()
        };
        headers.insert(name.to_owned(), "attacker-controlled".to_owned());
        match McpClient::connect(&config).await {
            Ok(client) => {
                client.shutdown().await;
                panic!("reserved header {name} must be rejected");
            }
            Err(error) => {
                let error = error.to_string().to_ascii_lowercase();
                assert!(
                    error.contains("strict mcp 2026 surface") || error.contains("reserved"),
                    "unexpected {name} error: {error}"
                );
            }
        }
        assert!(
            fixture.requests.lock().unwrap().is_empty(),
            "{name} reached wire"
        );
        fixture.shutdown().await;
    }
}

async fn assert_violation(behavior: RawBehavior) {
    isolate_environment();
    let fixture = RawHttpFixture::spawn(behavior).await;
    let client = McpClient::connect(&config(&fixture.url))
        .await
        .expect("connect fixture");
    client
        .list_all_tools()
        .await
        .expect_err("strict violation must fail");
    client
        .call_tool("echo", json!({}))
        .await
        .expect_err("connection must be poisoned");
    client.shutdown().await;
    let requests = fixture.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 2, "second operation must not reach wire");
    assert!(requests.iter().all(|request| request.http_method == "POST"));
    assert!(
        requests
            .iter()
            .all(|request| !request.headers.contains_key("last-event-id"))
    );
    fixture.shutdown().await;
}

async fn wait_for_requests(fixture: &RawHttpFixture, count: usize) {
    for _ in 0..80 {
        if fixture.requests.lock().unwrap().len() >= count {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("timed out waiting for HTTP request");
}

async fn wait_for_close(fixture: &RawHttpFixture) {
    for _ in 0..80 {
        if fixture.hang_closed.load(Ordering::Acquire) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("timed out waiting for HTTP connection close");
}
