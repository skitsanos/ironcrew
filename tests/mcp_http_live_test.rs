//! Live integration test for the MCP HTTP Streamable client.
//!
//! Connects to a real MCP 2026-07-28 server, lists tools, and invokes an
//! explicitly selected tool. These
//! tests hit the network and are `#[ignore]`d by default — run with:
//!
//!     cargo test --features mcp -- --ignored mcp_http_live
//!
//! Set `MCP_TEST_URL` to a known Streamable HTTP POST endpoint. The call test
//! also requires `MCP_TEST_TOOL`; optional `MCP_TEST_ARGUMENTS` defaults to
//! `{}`. There is intentionally no public or legacy default endpoint.

#![cfg(feature = "mcp")]

use ironcrew::mcp::{
    client::McpClient,
    config::{McpServerConfig, McpTransportConfig},
};
use std::collections::HashMap;
use std::time::Duration;

fn server_url() -> String {
    std::env::var("MCP_TEST_URL")
        .expect("MCP_TEST_URL must name a known MCP 2026-07-28 Streamable HTTP endpoint")
}

fn cfg(url: &str) -> McpServerConfig {
    McpServerConfig {
        label: "live".into(),
        transport: McpTransportConfig::Http {
            url: url.to_string(),
            headers: HashMap::new(),
        },
        execution_identity_fingerprint: None,
        inherit_env: false,
    }
}

#[tokio::test]
#[ignore]
async fn mcp_http_live_discovery_and_list_tools() {
    let url = server_url();
    let client = tokio::time::timeout(Duration::from_secs(15), McpClient::connect(&cfg(&url)))
        .await
        .expect("discovery timed out")
        .expect("connect failed");

    let tools = tokio::time::timeout(Duration::from_secs(15), client.list_all_tools())
        .await
        .expect("list_all_tools timed out")
        .expect("list_all_tools failed");

    assert!(!tools.is_empty(), "expected at least one tool from {}", url);
    eprintln!("Discovered {} tools from {}", tools.len(), url);
    for t in &tools {
        eprintln!("  - {}", t.name);
    }

    client.shutdown().await;
}

#[tokio::test]
#[ignore]
async fn mcp_http_live_call_tool() {
    let url = server_url();
    let client = tokio::time::timeout(Duration::from_secs(15), McpClient::connect(&cfg(&url)))
        .await
        .expect("discovery timed out")
        .expect("connect failed");

    let tool_name = std::env::var("MCP_TEST_TOOL")
        .expect("MCP_TEST_TOOL must name the explicitly approved tool to call");
    let arguments = std::env::var("MCP_TEST_ARGUMENTS")
        .map(|raw| serde_json::from_str(&raw).expect("MCP_TEST_ARGUMENTS must be JSON"))
        .unwrap_or_else(|_| serde_json::json!({}));

    eprintln!("Calling tool '{}'", tool_name);
    let result = tokio::time::timeout(
        Duration::from_secs(30),
        client.call_tool(&tool_name, arguments),
    )
    .await
    .expect("call_tool timed out")
    .expect("call_tool failed");

    assert!(
        !result.content.is_empty() || result.is_error.unwrap_or(false),
        "expected non-empty tool response"
    );
    eprintln!("Tool returned {} content block(s)", result.content.len());

    client.shutdown().await;
}
