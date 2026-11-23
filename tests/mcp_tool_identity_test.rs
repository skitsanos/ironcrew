#![cfg(all(feature = "mcp", unix))]

use std::{collections::HashMap, path::Path, sync::Once};

use ironcrew::{
    mcp::{
        client::McpClient,
        config::{McpConfig, McpServerConfig, McpTransportConfig},
        manager::McpConnectionManager,
    },
    tools::{ToolCallContext, hash::HashTool, registry::ToolRegistry},
};
use serde_json::{Value, json};
use tempfile::TempDir;

#[path = "mcp_2026_protocol/process_assertions.rs"]
mod process_assertions;

const FIXTURE: &str = r#"
import json,runpy,sys
scope = runpy.run_path(sys.argv[1])
scope['TOOLS'][:] = [dict(name=name, description='Synthetic tool',
    inputSchema=dict(type='object', properties={})) for name in json.loads(sys.argv[2])]
original = scope['handle']
def handle(request):
    response = original(request)
    if request['method'] == 'tools/list' and sys.argv[3] == 'pages':
        if not (request.get('params') or {}).get('cursor'):
            response['result']['nextCursor'] = 'second-page'
    return response
scope['main'].__globals__['handle'] = handle
sys.exit(scope['main']())
"#;

fn isolate_environment() {
    static INIT: Once = Once::new();
    INIT.call_once(|| unsafe {
        std::env::set_var("IRONCREW_MCP_DISCOVERY_TIMEOUT_SECS", "10");
        std::env::set_var("IRONCREW_MCP_LIST_TIMEOUT_SECS", "10");
        std::env::set_var("IRONCREW_MCP_SHUTDOWN_TIMEOUT_SECS", "5");
    });
}

fn config(temp: &TempDir, label: &str, names: &[&str], paginated: bool) -> McpServerConfig {
    McpServerConfig {
        label: label.into(),
        transport: McpTransportConfig::Stdio {
            command: "python3".into(),
            args: vec![
                "-c".into(),
                FIXTURE.into(),
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("examples/mcp/stdio-tools/server.py")
                    .display()
                    .to_string(),
                serde_json::to_string(names).unwrap(),
                if paginated { "pages" } else { "single" }.into(),
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
            ]),
        },
        execution_identity_fingerprint: Some("synthetic-v1".into()),
        inherit_env: false,
    }
}

async fn assert_reaped(temp: &TempDir) {
    process_assertions::assert_process_stopped(temp.path().join("server.pid")).await;
}

fn requests(temp: &TempDir) -> Vec<Value> {
    std::fs::read_to_string(temp.path().join("requests.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[tokio::test]
async fn duplicate_stdio_tools_on_one_or_multiple_pages_poison_and_reap() {
    isolate_environment();
    for paginated in [false, true] {
        let temp = TempDir::new().unwrap();
        let names = if paginated {
            vec!["echo"]
        } else {
            vec!["echo", "echo"]
        };
        let client = McpClient::connect(&config(&temp, "duplicate", &names, paginated))
            .await
            .unwrap();
        let result = client.list_all_tools().await;
        if result.is_ok() {
            client.shutdown().await;
            panic!("duplicate tool names must reject the listing");
        }
        assert!(result.unwrap_err().to_string().contains("duplicate tool"));
        assert_reaped(&temp).await;
        assert!(client.call_tool("echo", json!({})).await.is_err());
        assert!(requests(&temp).iter().all(|r| r["method"] != "tools/call"));
        client.shutdown().await;
    }
}

#[tokio::test]
async fn ambiguous_composed_names_reject_the_batch_before_registry_mutation() {
    isolate_environment();
    let first = TempDir::new().unwrap();
    let second = TempDir::new().unwrap();
    let config = McpConfig {
        servers: vec![
            config(&first, "a", &["unique", "b__c"], false),
            config(&second, "a__b", &["c"], false),
        ],
    };
    let mut registry = ToolRegistry::new();
    registry.register(Box::new(HashTool::new()));
    let error = match McpConnectionManager::connect_all(&config, &mut registry).await {
        Ok(manager) => {
            manager.shutdown().await;
            panic!("ambiguous mcp__a__b__c must reject the entire batch");
        }
        Err(error) => error.to_string(),
    };
    assert!(error.contains("mcp__a__b__c"), "{error}");
    assert_eq!(registry.list(), vec!["hash"]);
    assert_reaped(&first).await;
    assert_reaped(&second).await;
}

#[tokio::test]
async fn discovery_cannot_replace_a_tool_already_owned_by_another_manager() {
    isolate_environment();
    let first = TempDir::new().unwrap();
    let second = TempDir::new().unwrap();
    let mut registry = ToolRegistry::new();
    let first_config = McpConfig {
        servers: vec![config(&first, "owner", &["echo"], false)],
    };
    let original = McpConnectionManager::connect_all(&first_config, &mut registry)
        .await
        .unwrap();
    let definition = registry
        .get("mcp__owner__echo")
        .unwrap()
        .conversation_definition()
        .unwrap();
    let mut replacement = config(&second, "owner", &["echo"], false);
    replacement.execution_identity_fingerprint = Some("synthetic-v2".into());
    let error = match McpConnectionManager::connect_all(
        &McpConfig {
            servers: vec![replacement],
        },
        &mut registry,
    )
    .await
    {
        Ok(manager) => {
            manager.shutdown().await;
            original.shutdown().await;
            panic!("discovery must not overwrite a registered identity");
        }
        Err(error) => error.to_string(),
    };
    assert!(error.contains("mcp__owner__echo"), "{error}");
    assert_eq!(
        registry
            .get("mcp__owner__echo")
            .unwrap()
            .conversation_definition()
            .unwrap(),
        definition
    );
    assert_reaped(&second).await;
    let result = registry
        .get("mcp__owner__echo")
        .unwrap()
        .execute(json!({"text": "preserved"}), &ToolCallContext::default())
        .await
        .unwrap();
    assert!(result.contains("preserved"), "{result}");
    original.shutdown().await;
    assert_reaped(&first).await;
}

#[tokio::test]
async fn distinct_names_with_underscores_remain_supported() {
    isolate_environment();
    let first = TempDir::new().unwrap();
    let second = TempDir::new().unwrap();
    let mut registry = ToolRegistry::new();
    let config = McpConfig {
        servers: vec![
            config(&first, "a", &["b__c"], false),
            config(&second, "a_b", &["c"], false),
        ],
    };
    let manager = McpConnectionManager::connect_all(&config, &mut registry)
        .await
        .unwrap();
    let mut names = registry.list();
    names.sort();
    assert_eq!(names, vec!["mcp__a__b__c", "mcp__a_b__c"]);
    manager.shutdown().await;
    assert_reaped(&first).await;
    assert_reaped(&second).await;
}

#[tokio::test]
async fn duplicate_server_labels_fail_before_spawning_any_child() {
    isolate_environment();
    let first = TempDir::new().unwrap();
    let second = TempDir::new().unwrap();
    let mut registry = ToolRegistry::new();
    registry.register(Box::new(HashTool::new()));
    let config = McpConfig {
        servers: vec![
            config(&first, "same", &["first"], false),
            config(&second, "same", &["second"], false),
        ],
    };
    let error = match McpConnectionManager::connect_all(&config, &mut registry).await {
        Ok(manager) => {
            manager.shutdown().await;
            panic!("duplicate server labels must fail before connection");
        }
        Err(error) => error.to_string(),
    };
    assert!(error.contains("duplicate MCP server label"), "{error}");
    assert_eq!(registry.list(), vec!["hash"]);
    assert!(!first.path().join("server.pid").exists());
    assert!(!second.path().join("server.pid").exists());
}
