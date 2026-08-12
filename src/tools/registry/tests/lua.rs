use std::sync::Arc;

use super::support::assert_tool_policy_drift;
use crate::tools::lua_tool::LuaScriptTool;
use crate::tools::{Tool, ToolCallContext};

#[test]
fn lua_tool_roots_and_fs_limits_are_bound_without_exposing_paths() {
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    let tool = |read_root, write_root, read_limit, write_limit| {
        Box::new(LuaScriptTool::with_fs_policy_for_test(
            "lua_lookup".into(),
            "lookup".into(),
            serde_json::json!({"type": "object"}),
            Arc::from("return { execute = function() return 'ok' end }"),
            Some(read_root),
            Some(write_root),
            read_limit,
            write_limit,
            8_192,
            false,
        )) as Box<dyn Tool>
    };
    let left = tool(
        first.path().to_path_buf(),
        first.path().to_path_buf(),
        1_024,
        2_048,
    );
    let right = tool(
        second.path().to_path_buf(),
        second.path().to_path_buf(),
        2_048,
        4_096,
    );
    assert_tool_policy_drift(left, right, "lua_lookup");

    let definition = LuaScriptTool::with_fs_policy_for_test(
        "lua_lookup".into(),
        "lookup".into(),
        serde_json::json!({"type": "object"}),
        Arc::from("return { execute = function() return 'ok' end }"),
        Some(first.path().to_path_buf()),
        Some(first.path().to_path_buf()),
        1_024,
        2_048,
        8_192,
        false,
    )
    .conversation_definition()
    .unwrap()
    .to_string();
    assert!(!definition.contains(&first.path().display().to_string()));
}

#[tokio::test]
async fn lua_http_policy_drifts_and_execution_uses_the_captured_limit() {
    let source: Arc<str> = Arc::from(
        r#"
        return {
            execute = function()
                return http.post("https://example.invalid", { body = "12345" })
            end
        }
        "#,
    );
    let tool = |http_marker, allow_private| {
        LuaScriptTool::with_fs_policy_for_test(
            "lua_http".into(),
            "http".into(),
            serde_json::json!({"type": "object"}),
            source.clone(),
            None,
            None,
            1_024,
            1_024,
            http_marker,
            allow_private,
        )
    };
    let left = tool(4, false);
    let right = tool(8, true);
    assert_tool_policy_drift(Box::new(left), Box::new(right), "lua_http");

    let error = tool(4, false)
        .execute(serde_json::json!({}), &ToolCallContext::default())
        .await
        .expect_err("captured four-byte body limit must fail before network access");
    assert!(
        error.to_string().contains("(4)"),
        "unexpected error: {error}"
    );
}
