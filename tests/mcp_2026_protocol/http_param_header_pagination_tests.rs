use ironcrew::mcp::client::McpClient;
use serde_json::{Value, json};

use super::{
    boundary_test_support::isolate_environment,
    header_http_fixture::{HeaderHttpFixture, HeaderRequest, methods},
    header_mismatch_cases::HeaderMismatchCase,
    http_param_header_tests::config,
};

#[tokio::test]
async fn paginated_sse_refresh_commits_terminal_plan_before_one_retry() {
    isolate_environment();
    let fixture =
        HeaderHttpFixture::spawn_header_mismatch_sse(HeaderMismatchCase::PaginatedRetrySucceeds)
            .await;
    let client = McpClient::connect(&config(&fixture.url)).await.unwrap();
    client.list_all_tools().await.unwrap();
    client
        .call_tool("refresh", json!({"tenant": {"region": "eu-west-1"}}))
        .await
        .expect("terminal SSE page commits the complete refreshed plan");
    client.shutdown().await;

    let requests = fixture.requests.lock().unwrap().clone();
    assert_eq!(
        methods(&requests),
        [
            "server/discover",
            "tools/list",
            "tools/list",
            "tools/call",
            "tools/list",
            "tools/list",
            "tools/call"
        ]
    );
    let lists: Vec<_> = requests
        .iter()
        .filter(|request| request.body["method"] == "tools/list")
        .collect();
    assert!(cursor(lists[0]).is_none());
    assert_eq!(cursor(lists[1]), Some("initial-page-2"));
    assert!(cursor(lists[2]).is_none());
    assert_eq!(cursor(lists[3]), Some("refresh-page-2"));
    let calls = calls(&requests);
    assert_eq!(calls[0].header("mcp-param-stale"), "eu-west-1");
    assert_eq!(calls[1].header("mcp-param-current"), "eu-west-1");
    fixture.shutdown().await;
}

#[tokio::test]
async fn json_duplicate_on_later_refresh_page_poisons_and_blocks_later_wire() {
    isolate_environment();
    let fixture =
        HeaderHttpFixture::spawn_header_mismatch(HeaderMismatchCase::DuplicateRefreshPage).await;
    let client = McpClient::connect(&config(&fixture.url)).await.unwrap();
    client.list_all_tools().await.unwrap();
    let error = client
        .call_tool("refresh", json!({"tenant": {"region": "eu-west-1"}}))
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("duplicate tool `refresh`"), "{error}");
    client
        .call_tool("refresh", json!({"tenant": {"region": "must-not-send"}}))
        .await
        .expect_err("failed refresh must poison the connection");
    client.shutdown().await;

    let requests = fixture.requests.lock().unwrap().clone();
    assert_eq!(
        methods(&requests),
        [
            "server/discover",
            "tools/list",
            "tools/call",
            "tools/list",
            "tools/list"
        ]
    );
    let calls = calls(&requests);
    assert_eq!(calls.len(), 1, "a partially staged plan must never retry");
    assert_eq!(calls[0].header("mcp-param-stale"), "eu-west-1");
    assert_eq!(
        requests.len(),
        5,
        "poisoned connection must send no later call"
    );
    fixture.shutdown().await;
}

fn cursor(request: &HeaderRequest) -> Option<&str> {
    request
        .body
        .pointer("/params/cursor")
        .and_then(Value::as_str)
}

fn calls(requests: &[HeaderRequest]) -> Vec<&HeaderRequest> {
    requests
        .iter()
        .filter(|request| request.body["method"] == "tools/call")
        .collect()
}
