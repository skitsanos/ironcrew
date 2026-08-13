use std::collections::HashMap;

use ironcrew::{
    mcp::{
        client::McpClient,
        config::{McpServerConfig, McpTransportConfig},
    },
    utils::error::IronCrewError,
};
use serde_json::json;

use super::{
    boundary_test_support::isolate_environment,
    header_http_fixture::{HeaderHttpFixture, HeaderRequest, methods},
    header_mismatch_cases::HeaderMismatchCase,
    param_header_schemas::valid_tool,
};

fn configured(url: &str) -> McpServerConfig {
    McpServerConfig {
        label: "header-fixture".into(),
        transport: McpTransportConfig::Http {
            url: url.into(),
            headers: HashMap::from([
                ("Authorization".into(), "FixtureAuth".into()),
                ("X-Fixture-Context".into(), "configured".into()),
            ]),
        },
        execution_identity_fingerprint: Some("headers-v1".into()),
        inherit_env: false,
    }
}

#[tokio::test]
async fn configured_authorization_and_custom_headers_survive_param_promotion() {
    isolate_environment();
    let fixture = HeaderHttpFixture::spawn(json!([valid_tool()]), false).await;
    let client = McpClient::connect(&configured(&fixture.url)).await.unwrap();
    client.list_all_tools().await.unwrap();
    client
        .call_tool("promote", json!({"plain": "promoted"}))
        .await
        .unwrap();
    client.shutdown().await;
    let requests = fixture.requests.lock().unwrap().clone();
    assert_eq!(
        methods(&requests),
        ["server/discover", "tools/list", "tools/call"]
    );
    for request in &requests {
        assert_eq!(request.header("authorization"), "FixtureAuth");
        assert_eq!(request.header("x-fixture-context"), "configured");
    }
    assert_eq!(
        request(&requests, "tools/call").header("mcp-param-plain"),
        "promoted"
    );
    fixture.shutdown().await;
}

#[tokio::test]
async fn only_exact_header_mismatch_code_triggers_refresh() {
    isolate_environment();
    let fixture = HeaderHttpFixture::spawn_header_mismatch(HeaderMismatchCase::WrongCode).await;
    let client = McpClient::connect(&configured(&fixture.url)).await.unwrap();
    client.list_all_tools().await.unwrap();
    let error = client
        .call_tool("refresh", json!({"tenant": {"region": "eu-west-1"}}))
        .await
        .unwrap_err();
    assert_mcp_code(&error, "-32021");
    client
        .call_tool(
            "refresh",
            json!({"tenant": {"region": "explicit-second-call"}}),
        )
        .await
        .expect("ordinary tool errors do not poison the connection");
    client.shutdown().await;
    let requests = fixture.requests.lock().unwrap().clone();
    assert_eq!(
        methods(&requests),
        ["server/discover", "tools/list", "tools/call", "tools/call"]
    );
    fixture.shutdown().await;
}

#[tokio::test]
async fn missing_or_invalid_refresh_target_poisons_without_call_retry() {
    isolate_environment();
    for case in [
        HeaderMismatchCase::MissingRefreshTarget,
        HeaderMismatchCase::InvalidRefreshTarget,
    ] {
        let fixture = HeaderHttpFixture::spawn_header_mismatch(case).await;
        let client = McpClient::connect(&configured(&fixture.url)).await.unwrap();
        client.list_all_tools().await.unwrap();
        client
            .call_tool("refresh", json!({"tenant": {"region": "eu-west-1"}}))
            .await
            .expect_err("refresh cannot produce a retryable target");
        client
            .call_tool("refresh", json!({"tenant": {"region": "must-not-send"}}))
            .await
            .expect_err("failed refresh must poison the connection");
        client.shutdown().await;
        let requests = fixture.requests.lock().unwrap().clone();
        assert_eq!(
            methods(&requests),
            ["server/discover", "tools/list", "tools/call", "tools/list"]
        );
        fixture.shutdown().await;
    }
}

fn request<'a>(requests: &'a [HeaderRequest], method: &str) -> &'a HeaderRequest {
    requests
        .iter()
        .find(|request| request.body["method"] == method)
        .unwrap()
}

fn assert_mcp_code(error: &IronCrewError, code: &str) {
    match error {
        IronCrewError::Mcp { message, .. } => assert!(message.contains(code), "{message}"),
        other => panic!("expected MCP error carrying {code}, got {other}"),
    }
}
