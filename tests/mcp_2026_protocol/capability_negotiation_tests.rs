use std::collections::HashMap;

use super::{boundary_test_support::isolate_environment, http_fixture::HttpFixture};
use ironcrew::mcp::{
    client::McpClient,
    config::{McpServerConfig, McpTransportConfig},
};

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
