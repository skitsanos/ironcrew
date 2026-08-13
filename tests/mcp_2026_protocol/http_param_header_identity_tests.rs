use std::sync::Arc;

#[cfg(unix)]
use std::{collections::HashMap, path::Path};

#[cfg(unix)]
use ironcrew::mcp::config::{McpServerConfig, McpTransportConfig};
use ironcrew::{
    mcp::{bridge::McpBridgeTool, client::McpClient},
    tools::{Tool, registry::ToolRegistry},
};
use serde_json::json;

use super::{
    boundary_test_support::isolate_environment, header_http_fixture::HeaderHttpFixture,
    header_mismatch_cases::HeaderMismatchCase, http_param_header_tests::config,
};

#[cfg(unix)]
#[tokio::test]
async fn conversation_fingerprint_distinguishes_http_plan_from_stdio() {
    isolate_environment();
    let fixture = HeaderHttpFixture::spawn_header_mismatch(HeaderMismatchCase::RetrySucceeds).await;
    let http = Arc::new(McpClient::connect(&config(&fixture.url)).await.unwrap());
    let tool = http.list_all_tools().await.unwrap().remove(0);
    let stdio = Arc::new(McpClient::connect(&stdio_config()).await.unwrap());

    let (http_registry, http_name) = registry(&tool, Arc::clone(&http));
    let (stdio_registry, stdio_name) = registry(&tool, Arc::clone(&stdio));
    let http_fingerprint = fingerprint(&http_registry, &http_name);
    let stdio_fingerprint = fingerprint(&stdio_registry, &stdio_name);
    assert_ne!(
        http_fingerprint, stdio_fingerprint,
        "HTTP promotion behavior is part of durable conversation identity"
    );

    http.shutdown().await;
    stdio.shutdown().await;
    fixture.shutdown().await;
}

#[tokio::test]
async fn conversation_fingerprint_changes_after_committed_plan_change() {
    isolate_environment();
    let fixture = HeaderHttpFixture::spawn_header_mismatch(HeaderMismatchCase::RetrySucceeds).await;
    let client = Arc::new(McpClient::connect(&config(&fixture.url)).await.unwrap());
    let stale = client.list_all_tools().await.unwrap().remove(0);
    let (registry, name) = registry(&stale, Arc::clone(&client));
    let stale_fingerprint = fingerprint(&registry, &name);

    client
        .call_tool("refresh", json!({"tenant": {"region": "eu-west-1"}}))
        .await
        .expect("HeaderMismatch refresh commits a new plan");
    let current_fingerprint = fingerprint(&registry, &name);
    assert_ne!(
        stale_fingerprint, current_fingerprint,
        "committing a different x-mcp-header plan must fence durable replay"
    );

    client.shutdown().await;
    fixture.shutdown().await;
}

#[tokio::test]
async fn failed_paginated_refresh_keeps_previous_committed_fingerprint() {
    isolate_environment();
    let fixture =
        HeaderHttpFixture::spawn_header_mismatch(HeaderMismatchCase::DuplicateRefreshPage).await;
    let client = Arc::new(McpClient::connect(&config(&fixture.url)).await.unwrap());
    let stale = client.list_all_tools().await.unwrap().remove(0);
    let (registry, name) = registry(&stale, Arc::clone(&client));
    let before = fingerprint(&registry, &name);

    let error = client
        .call_tool("refresh", json!({"tenant": {"region": "eu-west-1"}}))
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("duplicate tool `refresh`"), "{error}");
    let after = fingerprint(&registry, &name);
    assert_eq!(
        before, after,
        "a failed nonterminal transaction must not replace the active plan"
    );

    client.shutdown().await;
    fixture.shutdown().await;
}

fn registry(tool: &rmcp::model::Tool, client: Arc<McpClient>) -> (ToolRegistry, String) {
    let bridge = McpBridgeTool::from_rmcp_tool(
        "identity-fixture",
        tool,
        client,
        Some("sha256:fixture-execution-identity".into()),
    )
    .unwrap();
    let name = bridge.name().to_owned();
    let mut registry = ToolRegistry::new();
    registry.register(Box::new(bridge));
    (registry, name)
}

fn fingerprint(registry: &ToolRegistry, name: &str) -> String {
    registry
        .conversation_execution_fingerprint(&[name.to_owned()])
        .unwrap()
}

#[cfg(unix)]
fn stdio_config() -> McpServerConfig {
    McpServerConfig {
        label: "identity-fixture".into(),
        transport: McpTransportConfig::Stdio {
            command: "python3".into(),
            args: vec![
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("examples/mcp/stdio-tools/server.py")
                    .display()
                    .to_string(),
            ],
            env: HashMap::new(),
        },
        execution_identity_fingerprint: Some("sha256:fixture-execution-identity".into()),
        inherit_env: false,
    }
}
