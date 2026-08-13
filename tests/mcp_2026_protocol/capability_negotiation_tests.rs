use std::collections::HashMap;

#[cfg(unix)]
use std::path::Path;

use ironcrew::mcp::{
    client::McpClient,
    config::{McpServerConfig, McpTransportConfig},
};
#[cfg(unix)]
use tempfile::TempDir;

#[cfg(unix)]
use super::stdio_test_support::assert_process_stopped;
use super::{boundary_test_support::isolate_environment, http_fixture::HttpFixture};

#[tokio::test]
async fn http_without_tools_capability_fails_before_tools_list_wire() {
    isolate_environment();
    let fixture = HttpFixture::spawn_without_tools_capability().await;
    let error = connect_must_fail(http_config(&fixture.url)).await;
    assert!(error.contains("tools capability"), "{error}");
    let requests = fixture.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].body["method"], "server/discover");
    fixture.shutdown().await;
}

#[cfg(unix)]
#[tokio::test]
async fn stdio_without_tools_capability_fails_before_tools_list_wire() {
    isolate_environment();
    let temp = TempDir::new().unwrap();
    let error = connect_must_fail(stdio_config(&temp)).await;
    assert!(error.contains("tools capability"), "{error}");
    let log = std::fs::read_to_string(temp.path().join("requests.jsonl")).unwrap();
    let requests: Vec<serde_json::Value> = log
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0]["method"], "server/discover");
    assert_process_stopped(temp.path().join("server.pid")).await;
}

async fn connect_must_fail(config: McpServerConfig) -> String {
    match McpClient::connect(&config).await {
        Ok(client) => {
            client.shutdown().await;
            panic!("server without the tools capability must be rejected")
        }
        Err(error) => error.to_string(),
    }
}

fn http_config(url: &str) -> McpServerConfig {
    McpServerConfig {
        label: "no-tools".into(),
        transport: McpTransportConfig::Http {
            url: url.into(),
            headers: HashMap::new(),
        },
        execution_identity_fingerprint: Some("no-tools-v1".into()),
        inherit_env: false,
    }
}

#[cfg(unix)]
fn stdio_config(temp: &TempDir) -> McpServerConfig {
    McpServerConfig {
        label: "no-tools".into(),
        transport: McpTransportConfig::Stdio {
            command: "python3".into(),
            args: vec![
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("examples/mcp/stdio-tools/server.py")
                    .display()
                    .to_string(),
            ],
            env: HashMap::from([
                (
                    "MCP_FIXTURE_LOG_FILE".into(),
                    temp.path().join("requests.jsonl").display().to_string(),
                ),
                (
                    "MCP_FIXTURE_PID_FILE".into(),
                    temp.path().join("server.pid").display().to_string(),
                ),
                ("MCP_FIXTURE_OMIT_TOOLS_CAPABILITY".into(), "1".into()),
            ]),
        },
        execution_identity_fingerprint: Some("no-tools-v1".into()),
        inherit_env: false,
    }
}

#[cfg(not(unix))]
#[tokio::test]
async fn stdio_is_rejected_before_process_spawn_on_unsupported_platforms() {
    isolate_environment();
    let config = McpServerConfig {
        label: "unsupported-stdio".into(),
        transport: McpTransportConfig::Stdio {
            command: "must-not-spawn".into(),
            args: Vec::new(),
            env: HashMap::new(),
        },
        execution_identity_fingerprint: Some("unsupported-v1".into()),
        inherit_env: false,
    };
    let error = connect_must_fail(config).await.to_ascii_lowercase();
    assert!(error.contains("stdio") && error.contains("unix"), "{error}");
}
