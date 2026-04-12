//! Live integration test for the MCP HTTP Streamable client.
//!
//! Connects to a real MCP server, lists tools, and invokes one. These
//! tests hit the network and are `#[ignore]`d by default — run with:
//!
//!     cargo test --features mcp -- --ignored mcp_http_live
//!
//! The default endpoint is the public PLU Finder MCP server. Override
//! with `MCP_TEST_URL` to point at another Streamable HTTP endpoint.

#![cfg(feature = "mcp")]

use ironcrew::mcp::{
    client::McpClient,
    config::{McpServerConfig, McpTransportConfig},
};
use std::collections::HashMap;
use std::time::Duration;

fn server_url() -> String {
    std::env::var("MCP_TEST_URL").unwrap_or_else(|_| "https://mcp.plufinder.com/sse".to_string())
}

fn cfg(url: &str) -> McpServerConfig {
    McpServerConfig {
        label: "plu".into(),
        transport: McpTransportConfig::Http {
            url: url.to_string(),
            headers: HashMap::new(),
        },
        inherit_env: false,
    }
}

#[tokio::test]
#[ignore]
async fn mcp_http_live_handshake_and_list_tools() {
    let url = server_url();
    let client = tokio::time::timeout(Duration::from_secs(15), McpClient::connect(&cfg(&url)))
        .await
        .expect("handshake timed out")
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
        .expect("handshake timed out")
        .expect("connect failed");

    let tools = client.list_all_tools().await.expect("list tools");

    // Pick a zero-arg or single-arg tool that looks safe to invoke. For the
    // PLU Finder, `get_plu_categories` takes no required args.
    let target = tools
        .iter()
        .find(|t| t.name == "get_plu_categories")
        .or_else(|| tools.first())
        .expect("no tools available");

    eprintln!("Calling tool '{}'", target.name);
    let result = tokio::time::timeout(
        Duration::from_secs(30),
        client.call_tool(&target.name, serde_json::json!({})),
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
