#![cfg(feature = "mcp")]

#[path = "mcp_2026_protocol/http_fixture.rs"]
mod http_fixture;

use std::collections::HashMap;
use std::sync::Once;

#[cfg(unix)]
use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use http_fixture::{CapturedRequest, HttpFixture};
use ironcrew::mcp::{
    client::McpClient,
    config::{McpServerConfig, McpTransportConfig},
};
use serde_json::{Value, json};
#[cfg(unix)]
use tempfile::TempDir;

const PROTOCOL_VERSION: &str = "2026-07-28";
const MAX_MRTR_ROUNDS: usize = 4;

fn isolate_test_environment() {
    static INIT: Once = Once::new();
    INIT.call_once(|| unsafe {
        std::env::set_var("IRONCREW_MCP_ALLOW_LOCALHOST", "1");
        std::env::set_var("IRONCREW_MCP_MAX_MRTR_ROUNDS", MAX_MRTR_ROUNDS.to_string());
        std::env::set_var("IRONCREW_MCP_MAX_REQUEST_STATE_BYTES", "65536");
        std::env::set_var("IRONCREW_MCP_CALL_TIMEOUT_SECS", "10");
        std::env::set_var("IRONCREW_MCP_DISCOVERY_TIMEOUT_SECS", "5");
        // The retired name is deliberately invalid; successful discovery proves
        // it is ignored rather than retained as a compatibility alias.
        std::env::set_var("IRONCREW_MCP_HANDSHAKE_TIMEOUT_SECS", "0");
        std::env::set_var("IRONCREW_MCP_LIST_TIMEOUT_SECS", "5");
        std::env::set_var("IRONCREW_MCP_SHUTDOWN_TIMEOUT_SECS", "5");
    });
}

#[cfg(unix)]
fn stdio_fixture_config(
    temp: &TempDir,
    legacy_only: bool,
    supported_version: Option<&str>,
) -> McpServerConfig {
    let mut env = HashMap::from([
        (
            "MCP_FIXTURE_LOG_FILE".to_owned(),
            temp.path().join("requests.jsonl").display().to_string(),
        ),
        (
            "MCP_FIXTURE_PID_FILE".to_owned(),
            temp.path().join("server.pid").display().to_string(),
        ),
    ]);
    if legacy_only {
        env.insert("MCP_FIXTURE_LEGACY_ONLY".to_owned(), "1".to_owned());
    }
    if let Some(version) = supported_version {
        env.insert(
            "MCP_FIXTURE_SUPPORTED_VERSION".to_owned(),
            version.to_owned(),
        );
    }
    McpServerConfig {
        label: "fixture".into(),
        transport: McpTransportConfig::Stdio {
            command: "python3".into(),
            args: vec![stdio_fixture_path().display().to_string()],
            env,
        },
        execution_identity_fingerprint: Some("fixture-v1".into()),
        inherit_env: false,
    }
}

fn http_fixture_config(url: &str) -> McpServerConfig {
    McpServerConfig {
        label: "fixture".into(),
        transport: McpTransportConfig::Http {
            url: url.into(),
            headers: HashMap::new(),
        },
        execution_identity_fingerprint: Some("fixture-v1".into()),
        inherit_env: false,
    }
}

#[cfg(unix)]
fn stdio_fixture_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/mcp/stdio-tools/server.py")
}

#[cfg(unix)]
fn read_stdio_log(temp: &TempDir) -> Vec<Value> {
    std::fs::read_to_string(temp.path().join("requests.jsonl"))
        .expect("read fixture log")
        .lines()
        .map(|line| serde_json::from_str(line).expect("parse fixture log line"))
        .collect()
}

fn result_text(result: &rmcp::model::CallToolResult) -> &str {
    result
        .content
        .first()
        .and_then(|content| content.as_text())
        .map(|text| text.text.as_str())
        .expect("text result")
}

#[cfg(unix)]
#[tokio::test]
async fn stdio_uses_discovery_and_drives_only_bounded_state_mrtr() {
    isolate_test_environment();
    let temp = TempDir::new().expect("temporary fixture directory");
    let client = McpClient::connect(&stdio_fixture_config(&temp, false, None))
        .await
        .expect("connect to MCP 2026 fixture");

    let tools = client.list_all_tools().await.expect("list tools");
    assert_eq!(tools.len(), 11);

    let echo = client
        .call_tool("echo", json!({"text": "wire-ok"}))
        .await
        .expect("call echo");
    assert_eq!(result_text(&echo), "wire-ok");

    let stateful = client
        .call_tool("stateful_echo", json!({}))
        .await
        .expect("drive state-only MRTR");
    assert_eq!(result_text(&stateful), "state-echo-ok");

    let empty_state = client
        .call_tool("empty_state", json!({}))
        .await
        .expect("echo empty requestState");
    assert_eq!(result_text(&empty_state), "empty-state-ok");

    client.shutdown().await;

    let requests = read_stdio_log(&temp);
    assert_eq!(requests[0]["method"], "server/discover");
    assert!(requests.iter().all(|entry| entry["method"] != "initialize"));
    assert_eq!(call_count(&requests, "stateful_echo"), 2);
    assert_eq!(call_count(&requests, "empty_state"), 2);
    assert_process_stopped(temp.path().join("server.pid")).await;
}

#[cfg(unix)]
#[tokio::test]
async fn stdio_does_not_fall_back_to_initialize() {
    isolate_test_environment();
    let temp = TempDir::new().expect("temporary fixture directory");
    let error = match McpClient::connect(&stdio_fixture_config(&temp, true, None)).await {
        Ok(client) => {
            client.shutdown().await;
            panic!("legacy-only server must be rejected");
        }
        Err(error) => error.to_string(),
    };
    assert!(error.contains("MCP stdio discovery failed"));

    let requests = read_stdio_log(&temp);
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0]["method"], "server/discover");
    assert_process_stopped(temp.path().join("server.pid")).await;
}

#[cfg(unix)]
#[tokio::test]
async fn stdio_rejects_discovery_without_the_2026_revision() {
    isolate_test_environment();
    let temp = TempDir::new().expect("temporary fixture directory");
    let config = stdio_fixture_config(&temp, false, Some("2025-11-25"));
    let error = match McpClient::connect(&config).await {
        Ok(client) => {
            client.shutdown().await;
            panic!("server without 2026-07-28 support must be rejected");
        }
        Err(error) => error.to_string(),
    };
    assert!(error.contains("MCP stdio discovery failed"));

    let requests = read_stdio_log(&temp);
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0]["method"], "server/discover");
    assert_process_stopped(temp.path().join("server.pid")).await;
}

#[tokio::test]
async fn http_is_sessionless_and_sends_2026_metadata_and_standard_headers() {
    isolate_test_environment();
    let fixture = HttpFixture::spawn(false).await;
    let client = McpClient::connect(&http_fixture_config(&fixture.url))
        .await
        .expect("connect to HTTP fixture");
    let tools = client.list_all_tools().await.expect("list HTTP tools");
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, "echo");
    let result = client
        .call_tool("echo", json!({"text": "http-ok"}))
        .await
        .expect("call HTTP tool");
    assert_eq!(result_text(&result), "http-ok");
    client.shutdown().await;

    let requests = fixture.requests.lock().expect("captured requests").clone();
    assert_eq!(
        request_methods(&requests),
        ["server/discover", "tools/list", "tools/call"]
    );
    for request in &requests {
        assert_eq!(request.http_method, axum::http::Method::POST);
        assert_eq!(header(request, "mcp-protocol-version"), PROTOCOL_VERSION);
        assert_eq!(header(request, "mcp-method"), request.body["method"]);
        assert!(!request.headers.contains_key("mcp-session-id"));
        assert_2026_metadata(&request.body);
    }
    assert!(!requests[0].headers.contains_key("mcp-name"));
    assert!(!requests[1].headers.contains_key("mcp-name"));
    assert_eq!(header(&requests[2], "mcp-name"), "echo");
    fixture.shutdown().await;
}

#[tokio::test]
async fn http_does_not_fall_back_or_create_a_legacy_session() {
    isolate_test_environment();
    let fixture = HttpFixture::spawn(true).await;
    let error = match McpClient::connect(&http_fixture_config(&fixture.url)).await {
        Ok(client) => {
            client.shutdown().await;
            panic!("legacy-only HTTP server must be rejected");
        }
        Err(error) => error.to_string(),
    };
    assert!(error.contains("MCP HTTP discovery failed"));

    let requests = fixture.requests.lock().expect("captured requests").clone();
    assert_eq!(request_methods(&requests), ["server/discover"]);
    assert!(!requests[0].headers.contains_key("mcp-session-id"));
    fixture.shutdown().await;
}

#[cfg(unix)]
fn call_count(requests: &[Value], name: &str) -> usize {
    requests
        .iter()
        .filter(|entry| entry["method"] == "tools/call" && entry["name"] == name)
        .count()
}

fn request_methods(requests: &[CapturedRequest]) -> Vec<&str> {
    requests
        .iter()
        .map(|request| request.body["method"].as_str().expect("request method"))
        .collect()
}

fn header<'a>(request: &'a CapturedRequest, name: &str) -> &'a str {
    request.headers.get(name).map(String::as_str).expect(name)
}

fn assert_2026_metadata(body: &Value) {
    let meta = &body["params"]["_meta"];
    assert_eq!(
        meta["io.modelcontextprotocol/protocolVersion"],
        PROTOCOL_VERSION
    );
    assert_eq!(
        meta["io.modelcontextprotocol/clientInfo"]["name"],
        "ironcrew"
    );
    let capabilities = meta["io.modelcontextprotocol/clientCapabilities"]
        .as_object()
        .expect("client capabilities object");
    assert!(
        capabilities.is_empty(),
        "IronCrew must not advertise optional client capabilities"
    );
}

#[cfg(unix)]
async fn assert_process_stopped(pid_file: PathBuf) {
    let pid = std::fs::read_to_string(pid_file)
        .expect("read fixture pid")
        .parse::<i32>()
        .expect("parse fixture pid");
    let pid = nix::unistd::Pid::from_raw(pid);
    for _ in 0..40 {
        if matches!(
            nix::sys::signal::kill(pid, None),
            Err(nix::errno::Errno::ESRCH)
        ) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("MCP fixture process {pid} is still alive");
}
