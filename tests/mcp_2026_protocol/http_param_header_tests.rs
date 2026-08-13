use std::collections::{HashMap, HashSet};

use base64::{Engine, prelude::BASE64_STANDARD};
use ironcrew::mcp::{
    client::McpClient,
    config::{McpServerConfig, McpTransportConfig},
};
use serde_json::json;

use super::boundary_test_support::isolate_environment;
use super::header_http_fixture::{HeaderHttpFixture, HeaderRequest, methods};
use super::header_mismatch_cases::HeaderMismatchCase;
use super::param_header_schemas::{invalid_and_valid_tools, valid_tool};

pub(super) fn config(url: &str) -> McpServerConfig {
    McpServerConfig {
        label: "header-fixture".into(),
        transport: McpTransportConfig::Http {
            url: url.into(),
            headers: HashMap::new(),
        },
        execution_identity_fingerprint: Some("headers-v1".into()),
        inherit_env: false,
    }
}

#[tokio::test]
async fn exact_param_headers_cover_nested_paths_types_encoding_and_omission() {
    isolate_environment();
    for list_sse in [false, true] {
        let expected_tool = valid_tool();
        let fixture = HeaderHttpFixture::spawn(json!([expected_tool.clone()]), list_sse).await;
        let client = McpClient::connect(&config(&fixture.url)).await.unwrap();
        let tools = client.list_all_tools().await.unwrap();
        assert_eq!(tool_names(&tools), HashSet::from(["promote"]));
        assert_eq!(
            serde_json::Value::Object(tools[0].input_schema.as_ref().clone()),
            expected_tool["inputSchema"],
            "returned tools must preserve the original annotated schema"
        );
        client
            .call_tool(
                "promote",
                json!({
                    "plain": "ascii value",
                    "x-mcp-header": "literal-property",
                    "tenant": {
                        "region": "eu-west-1",
                        "enabled": true,
                        "quota": 9_007_199_254_740_991_i64,
                        "nullable": null,
                        "sentinel": "=?base64?already?=",
                        "unicode": "café",
                        "control": "line1\nline2",
                        "padded": " leading"
                    }
                }),
            )
            .await
            .unwrap();
        client.shutdown().await;

        let requests = fixture.requests.lock().unwrap().clone();
        let call = request(&requests, "tools/call");
        assert_eq!(call.header("mcp-param-plain"), "ascii value");
        assert_eq!(call.header("mcp-param-literal"), "literal-property");
        assert_eq!(call.header("mcp-param-region"), "eu-west-1");
        assert_eq!(call.header("mcp-param-enabled"), "true");
        assert_eq!(call.header("mcp-param-quota"), "9007199254740991");
        assert_eq!(
            call.header("mcp-param-sentinel"),
            encoded("=?base64?already?=")
        );
        assert_eq!(call.header("mcp-param-unicode"), encoded("café"));
        assert_eq!(call.header("mcp-param-control"), encoded("line1\nline2"));
        assert_eq!(call.header("mcp-param-padded"), encoded(" leading"));
        let param_names: HashSet<_> = call
            .headers
            .keys()
            .filter(|name| name.starts_with("mcp-param-"))
            .map(String::as_str)
            .collect();
        assert_eq!(
            param_names,
            HashSet::from([
                "mcp-param-plain",
                "mcp-param-literal",
                "mcp-param-region",
                "mcp-param-enabled",
                "mcp-param-quota",
                "mcp-param-sentinel",
                "mcp-param-unicode",
                "mcp-param-control",
                "mcp-param-padded"
            ])
        );
        for name in &param_names {
            assert_eq!(
                call.header_count(name),
                1,
                "generated parameter header {name} must appear exactly once"
            );
        }
        assert!(!call.headers.contains_key("mcp-param-omitted"));
        assert!(!call.headers.contains_key("mcp-param-nullable"));
        fixture.shutdown().await;
    }
}

#[tokio::test]
async fn invalid_annotation_tools_are_excluded_without_hiding_valid_sibling() {
    isolate_environment();
    let fixture = HeaderHttpFixture::spawn(invalid_and_valid_tools(), false).await;
    let client = McpClient::connect(&config(&fixture.url)).await.unwrap();
    let tools = client.list_all_tools().await.unwrap();
    assert_eq!(tool_names(&tools), HashSet::from(["valid_sibling"]));
    client
        .call_tool("valid_sibling", json!({"value": "still-routable"}))
        .await
        .unwrap();
    client.shutdown().await;
    let requests = fixture.requests.lock().unwrap().clone();
    let call = request(&requests, "tools/call");
    assert_eq!(call.header("mcp-param-valid"), "still-routable");
    fixture.shutdown().await;
}

#[tokio::test]
async fn header_mismatch_refreshes_schema_once_and_retries_with_current_headers() {
    isolate_environment();
    let fixture = HeaderHttpFixture::spawn_header_mismatch(HeaderMismatchCase::RetrySucceeds).await;
    let client = McpClient::connect(&config(&fixture.url)).await.unwrap();
    client.list_all_tools().await.unwrap();
    client
        .call_tool("refresh", json!({"tenant": {"region": "eu-west-1"}}))
        .await
        .expect("one schema refresh makes the retry succeed");
    client.shutdown().await;
    let requests = fixture.requests.lock().unwrap().clone();
    assert_eq!(
        methods(&requests),
        [
            "server/discover",
            "tools/list",
            "tools/call",
            "tools/list",
            "tools/call"
        ]
    );
    let calls: Vec<_> = requests
        .iter()
        .filter(|request| request.body["method"] == "tools/call")
        .collect();
    assert_eq!(calls[0].header("mcp-param-stale"), "eu-west-1");
    assert!(!calls[0].headers.contains_key("mcp-param-current"));
    assert_eq!(calls[1].header("mcp-param-current"), "eu-west-1");
    assert!(!calls[1].headers.contains_key("mcp-param-stale"));
    fixture.shutdown().await;
}

#[tokio::test]
async fn repeated_header_mismatch_poisons_after_one_retry_and_sends_no_third_call() {
    isolate_environment();
    let fixture = HeaderHttpFixture::spawn_header_mismatch(HeaderMismatchCase::Repeated).await;
    let client = McpClient::connect(&config(&fixture.url)).await.unwrap();
    client.list_all_tools().await.unwrap();
    let error = client
        .call_tool("refresh", json!({"tenant": {"region": "eu-west-1"}}))
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("-32020"), "{error}");
    client
        .call_tool("refresh", json!({"tenant": {"region": "must-not-send"}}))
        .await
        .expect_err("repeated mismatch must poison the connection");
    client.shutdown().await;
    let requests = fixture.requests.lock().unwrap().clone();
    assert_eq!(
        methods(&requests),
        [
            "server/discover",
            "tools/list",
            "tools/call",
            "tools/list",
            "tools/call"
        ]
    );
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.body["method"] == "tools/call")
            .count(),
        2
    );
    fixture.shutdown().await;
}

fn tool_names(tools: &[rmcp::model::Tool]) -> HashSet<&str> {
    tools.iter().map(|tool| tool.name.as_ref()).collect()
}

fn request<'a>(requests: &'a [HeaderRequest], method: &str) -> &'a HeaderRequest {
    requests
        .iter()
        .find(|request| request.body["method"] == method)
        .unwrap()
}

fn encoded(value: &str) -> String {
    format!("=?base64?{}?=", BASE64_STANDARD.encode(value))
}
