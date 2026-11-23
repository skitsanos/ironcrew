#![cfg(all(feature = "mcp", unix))]

use std::collections::HashMap;
use std::path::Path;
use std::sync::Once;

use ironcrew::mcp::{
    client::McpClient,
    config::{McpServerConfig, McpTransportConfig},
};
use tempfile::TempDir;

#[path = "mcp_2026_protocol/process_assertions.rs"]
mod process_assertions;

// A separate integration-test process owns this functional budget. The boundary
// target retains its one-second timeout/cancellation policy without env races.
fn isolate_environment() {
    static INIT: Once = Once::new();
    INIT.call_once(|| unsafe {
        std::env::set_var("IRONCREW_MCP_DISCOVERY_TIMEOUT_SECS", "10");
        std::env::set_var("IRONCREW_MCP_SHUTDOWN_TIMEOUT_SECS", "5");
    });
}

#[tokio::test]
async fn missing_tools_capability_rejects_before_tools_list_and_reaps_child() {
    assert_capability_rejection(0).await;
}

#[tokio::test]
async fn delayed_interpreter_startup_still_checks_capability_before_tools_list() {
    // Exceeds the old shared deadline deterministically; this is semantic
    // acceptance, independent of the one-second discovery-timeout tests.
    assert_capability_rejection(1_200).await;
}

async fn assert_capability_rejection(startup_delay_ms: u64) {
    isolate_environment();
    let temp = TempDir::new().unwrap();
    let config = config(&temp, startup_delay_ms);
    let error = match McpClient::connect(&config).await {
        Ok(client) => {
            client.shutdown().await;
            panic!("server without tools capability must be rejected");
        }
        Err(error) => error.to_string(),
    };
    assert!(error.contains("tools capability"), "{error}");
    assert!(!error.contains("timed out"), "{error}");
    let log = std::fs::read_to_string(temp.path().join("requests.jsonl")).unwrap();
    let requests: Vec<serde_json::Value> = log
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(requests.len(), 1, "tools/list must never reach the wire");
    assert_eq!(requests[0]["method"], "server/discover");
    process_assertions::assert_process_stopped(temp.path().join("server.pid")).await;
}

fn config(temp: &TempDir, startup_delay_ms: u64) -> McpServerConfig {
    McpServerConfig {
        label: "no-tools".into(),
        transport: McpTransportConfig::Stdio {
            command: "python3".into(),
            args: vec![
                "-c".into(),
                "import runpy,sys,time; time.sleep(int(sys.argv[1])/1000); runpy.run_path(sys.argv[2], run_name='__main__')".into(),
                startup_delay_ms.to_string(),
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
