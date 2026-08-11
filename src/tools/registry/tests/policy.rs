use std::collections::HashMap;

use super::super::ToolRegistry;
use super::support::assert_tool_policy_drift;

#[test]
fn runtime_roots_change_selected_file_tool_identity_before_execution() {
    use crate::engine::runtime::Runtime;
    use crate::llm::openai::OpenAiProvider;

    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    let first_runtime = Runtime::new(
        Box::new(OpenAiProvider::new("not-used".into(), None)),
        Some(first.path()),
    );
    let second_runtime = Runtime::new(
        Box::new(OpenAiProvider::new("also-not-used".into(), None)),
        Some(second.path()),
    );

    for name in ["file_read", "file_read_glob"] {
        let selected = vec![name.to_string()];
        assert_ne!(
            first_runtime
                .tool_registry
                .conversation_execution_fingerprint(&selected)
                .unwrap(),
            second_runtime
                .tool_registry
                .conversation_execution_fingerprint(&selected)
                .unwrap(),
            "different runtime roots must change {name} identity"
        );
    }
}

#[test]
fn global_timeout_and_depth_policy_fence_every_registry() {
    let fingerprint = |timeout, depth| {
        ToolRegistry {
            tools: HashMap::new(),
            execution_policy: crate::tools::runtime_policy::RuntimeExecutionPolicy::from_values(
                timeout, depth,
            ),
            approval: None,
        }
        .conversation_execution_fingerprint(&[])
        .unwrap()
    };
    let baseline = fingerprint(60, 5);
    assert_ne!(baseline, fingerprint(61, 5));
    assert_ne!(baseline, fingerprint(60, 6));

    let runtime_limits = |marker, allow_private| {
        ToolRegistry {
            tools: HashMap::new(),
            execution_policy: crate::tools::runtime_policy::RuntimeExecutionPolicy::from_values(
                60, 5,
            )
            .with_lua_marker(marker, allow_private),
            approval: None,
        }
        .conversation_execution_fingerprint(&[])
        .unwrap()
    };
    assert_ne!(runtime_limits(1_024, false), runtime_limits(2_048, false));
    assert_ne!(runtime_limits(1_024, false), runtime_limits(1_024, true));

    let conversation_limits = |marker| {
        ToolRegistry {
            tools: HashMap::new(),
            execution_policy: crate::tools::runtime_policy::RuntimeExecutionPolicy::from_values(
                60, 5,
            )
            .with_conversation_marker(marker),
            approval: None,
        }
        .conversation_execution_fingerprint(&[])
        .unwrap()
    };
    assert_ne!(conversation_limits(1_024), conversation_limits(2_048));
}

#[tokio::test]
async fn captured_conversation_timeout_controls_the_actual_deadline() {
    let registry = ToolRegistry {
        tools: HashMap::new(),
        execution_policy: crate::tools::runtime_policy::RuntimeExecutionPolicy::from_values(60, 5)
            .with_conversation_marker(0),
        approval: None,
    };
    let result = tokio::time::timeout(
        registry.conversation_turn_timeout(),
        std::future::pending::<()>(),
    )
    .await;
    assert!(
        result.is_err(),
        "captured zero deadline must fire immediately"
    );
}

#[test]
fn builtin_limits_capabilities_and_network_policy_fence_before_dispatch() {
    use crate::tools::file_read::FileReadTool;
    use crate::tools::file_read_glob::FileReadGlobTool;
    use crate::tools::file_write::FileWriteTool;
    use crate::tools::http_request::HttpRequestTool;
    use crate::tools::shell::ShellTool;
    use crate::tools::validate_schema::ValidateSchemaTool;
    use crate::tools::web_scrape::WebScrapeTool;

    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();

    assert_tool_policy_drift(
        Box::new(FileReadTool::with_max_bytes_for_test(
            first.path().to_path_buf(),
            1_024,
        )),
        Box::new(FileReadTool::with_max_bytes_for_test(
            first.path().to_path_buf(),
            2_048,
        )),
        "file_read",
    );
    assert_tool_policy_drift(
        Box::new(FileReadGlobTool::with_limits_for_test(
            first.path().to_path_buf(),
            1_024,
        )),
        Box::new(FileReadGlobTool::with_limits_for_test(
            second.path().to_path_buf(),
            2_048,
        )),
        "file_read_glob",
    );
    assert_tool_policy_drift(
        Box::new(FileWriteTool::with_policy_for_test(
            first.path().to_path_buf(),
            vec!["txt".into()],
            1_024,
        )),
        Box::new(FileWriteTool::with_policy_for_test(
            second.path().to_path_buf(),
            vec!["json".into()],
            2_048,
        )),
        "file_write",
    );
    assert_tool_policy_drift(
        Box::new(ShellTool::with_policy_for_test(30, 1_024)),
        Box::new(ShellTool::with_policy_for_test(45, 2_048)),
        "shell",
    );
    assert_tool_policy_drift(
        Box::new(HttpRequestTool::with_policy_for_test(1_024, false)),
        Box::new(HttpRequestTool::with_policy_for_test(2_048, true)),
        "http_request",
    );
    assert_tool_policy_drift(
        Box::new(WebScrapeTool::with_policy_for_test(
            Some(vec!["example.test".into()]),
            1_024,
            false,
        )),
        Box::new(WebScrapeTool::with_policy_for_test(
            Some(vec!["*.example.test".into()]),
            2_048,
            true,
        )),
        "web_scrape",
    );
    assert_tool_policy_drift(
        Box::new(ValidateSchemaTool::with_limit_for_test(1_024)),
        Box::new(ValidateSchemaTool::with_limit_for_test(2_048)),
        "validate_schema",
    );
}
