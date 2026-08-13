use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use ironcrew::mcp::{
    client::McpClient,
    config::{McpServerConfig, McpTransportConfig},
};
use serde_json::json;
use tempfile::TempDir;

use super::boundary_test_support::isolate_environment;
use super::stdio_test_support::{
    assert_process_stopped, call_count, wait_for_file, wait_for_request,
};

fn config(
    temp: &TempDir,
    extra: impl IntoIterator<Item = (&'static str, String)>,
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
    env.extend(
        extra
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value)),
    );
    McpServerConfig {
        label: "boundary-fixture".into(),
        transport: McpTransportConfig::Stdio {
            command: "python3".into(),
            args: vec![
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("examples/mcp/stdio-tools/server.py")
                    .display()
                    .to_string(),
            ],
            env,
        },
        execution_identity_fingerprint: Some("boundary-v1".into()),
        inherit_env: false,
    }
}

fn process_group_config(
    temp: &TempDir,
    extra: &[(&'static str, String)],
) -> (McpServerConfig, PathBuf) {
    let grandchild_pid = temp.path().join("grandchild.pid");
    let mut vars = vec![
        ("MCP_FIXTURE_SPAWN_GRANDCHILD", "1".to_owned()),
        (
            "MCP_FIXTURE_GRANDCHILD_PID_FILE",
            grandchild_pid.display().to_string(),
        ),
    ];
    vars.extend_from_slice(extra);
    (config(temp, vars), grandchild_pid)
}

#[cfg(unix)]
#[tokio::test]
async fn oversized_frame_reaps_child_and_descendant() {
    isolate_environment();
    let temp = TempDir::new().unwrap();
    let (config, grandchild_pid) = process_group_config(&temp, &[]);
    let client = McpClient::connect(&config).await.expect("connect fixture");
    wait_for_file(&grandchild_pid).await;
    let error = client
        .call_tool("oversized_frame", json!({}))
        .await
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("connection") || error.contains("transport") || error.contains("closed")
    );
    client.shutdown().await;
    assert_process_stopped(temp.path().join("server.pid")).await;
    assert_process_stopped(grandchild_pid).await;
}

#[cfg(unix)]
#[tokio::test]
async fn partial_eof_poisons_connection_and_reaps_process_group() {
    isolate_environment();
    let temp = TempDir::new().unwrap();
    let (config, grandchild_pid) = process_group_config(&temp, &[]);
    let client = McpClient::connect(&config).await.expect("connect fixture");
    wait_for_file(&grandchild_pid).await;
    let error = client
        .call_tool("partial_eof", json!({}))
        .await
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("connection") || error.contains("transport") || error.contains("closed"),
        "unexpected partial-frame error: {error}"
    );
    client
        .call_tool("echo", json!({"text": "must-not-reach-wire"}))
        .await
        .expect_err("partial frame must poison the connection");
    client.shutdown().await;
    assert_eq!(call_count(&temp, "echo"), 0);
    assert_process_stopped(temp.path().join("server.pid")).await;
    assert_process_stopped(grandchild_pid).await;
}

#[cfg(unix)]
#[tokio::test]
async fn aborting_hanging_call_reaps_group_and_poison_blocks_second_call() {
    isolate_environment();
    let temp = TempDir::new().unwrap();
    let (config, grandchild_pid) = process_group_config(&temp, &[]);
    let client = Arc::new(McpClient::connect(&config).await.expect("connect fixture"));
    wait_for_file(&grandchild_pid).await;
    let active = Arc::clone(&client);
    let call = tokio::spawn(async move { active.call_tool("hang", json!({})).await });
    wait_for_request(&temp, Some("hang")).await;
    call.abort();
    let _ = call.await;
    assert_process_stopped(temp.path().join("server.pid")).await;
    assert_process_stopped(grandchild_pid).await;
    client
        .call_tool("echo", json!({"text": "blocked"}))
        .await
        .unwrap_err();
    assert_eq!(call_count(&temp, "echo"), 0);
    client.shutdown().await;
}

#[cfg(unix)]
#[tokio::test]
async fn aborting_hanging_list_reaps_child_and_descendant() {
    isolate_environment();
    let temp = TempDir::new().unwrap();
    let (config, grandchild_pid) =
        process_group_config(&temp, &[("MCP_FIXTURE_HANG_LIST", "1".to_owned())]);
    let client = Arc::new(McpClient::connect(&config).await.expect("connect fixture"));
    wait_for_file(&grandchild_pid).await;
    let active = Arc::clone(&client);
    let list = tokio::spawn(async move { active.list_all_tools().await });
    wait_for_request(&temp, None).await;
    list.abort();
    let _ = list.await;
    assert_process_stopped(temp.path().join("server.pid")).await;
    assert_process_stopped(grandchild_pid).await;
    client.shutdown().await;
}

#[cfg(unix)]
#[tokio::test]
async fn natural_child_exit_reaps_same_group_descendant() {
    isolate_environment();
    let temp = TempDir::new().unwrap();
    let (config, grandchild_pid) = process_group_config(
        &temp,
        &[("MCP_FIXTURE_EXIT_AFTER_DISCOVER", "1".to_owned())],
    );
    let client = McpClient::connect(&config)
        .await
        .expect("discovery completes before natural exit");
    wait_for_file(&grandchild_pid).await;
    assert_process_stopped(temp.path().join("server.pid")).await;
    assert_process_stopped(grandchild_pid).await;
    client.shutdown().await;
}

#[cfg(unix)]
#[tokio::test]
async fn invalid_results_poison_and_block_later_wire() {
    isolate_environment();
    for (name, expected, attempts) in [
        ("loop_forever", "exceeded the 4-round MRTR limit", 4),
        ("oversized_state", "requestState exceeds 65536 bytes", 1),
        (
            "input_request",
            "capabilities IronCrew did not advertise",
            1,
        ),
        (
            "empty_input",
            "without usable inputRequests or requestState",
            1,
        ),
        ("task", "io.modelcontextprotocol/tasks capability", 1),
    ] {
        let temp = TempDir::new().unwrap();
        let client = McpClient::connect(&config(&temp, []))
            .await
            .expect("connect fixture");
        let error = client
            .call_tool(name, json!({}))
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains(expected), "unexpected {name} error: {error}");
        client
            .call_tool("echo", json!({"text": "must-not-reach-wire"}))
            .await
            .expect_err("protocol violation must poison connection");
        client.shutdown().await;
        assert_eq!(call_count(&temp, name), attempts);
        assert_eq!(call_count(&temp, "echo"), 0);
        assert_process_stopped(temp.path().join("server.pid")).await;
    }
}

#[cfg(unix)]
#[tokio::test]
async fn configured_call_timeout_reaps_group_and_blocks_later_wire() {
    isolate_environment();
    let temp = TempDir::new().unwrap();
    let (config, grandchild_pid) = process_group_config(&temp, &[]);
    let client = McpClient::connect(&config).await.expect("connect fixture");
    let error = client
        .call_tool("hang", json!({}))
        .await
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("configured deadline"),
        "unexpected timeout: {error}"
    );
    assert_process_stopped(temp.path().join("server.pid")).await;
    assert_process_stopped(grandchild_pid).await;
    client.call_tool("echo", json!({})).await.unwrap_err();
    assert_eq!(call_count(&temp, "echo"), 0);
    client.shutdown().await;
}

#[cfg(unix)]
#[tokio::test]
async fn discovery_timeout_and_caller_abort_reap_process_groups() {
    isolate_environment();
    for abort in [false, true] {
        let temp = TempDir::new().unwrap();
        let (config, grandchild_pid) =
            process_group_config(&temp, &[("MCP_FIXTURE_HANG_DISCOVERY", "1".to_owned())]);
        if abort {
            let connect = tokio::spawn(async move { McpClient::connect(&config).await });
            wait_for_request(&temp, Some("__discover__")).await;
            connect.abort();
            let _ = connect.await;
        } else {
            let error = match McpClient::connect(&config).await {
                Ok(client) => {
                    client.shutdown().await;
                    panic!("hanging discovery must time out")
                }
                Err(error) => error.to_string(),
            };
            assert!(error.contains("discovery timed out"));
        }
        wait_for_file(&grandchild_pid).await;
        assert_process_stopped(temp.path().join("server.pid")).await;
        assert_process_stopped(grandchild_pid).await;
    }
}
