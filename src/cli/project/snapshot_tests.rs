use std::fs;
use std::path::Path;
use std::sync::Arc;

use mlua::AnyUserData;
use tempfile::TempDir;

use super::setup_http_conversation_runtime;
use crate::engine::conversation_definition::capture_flow_source;
use crate::lua::api::{CHAT_CREW_REGISTRY_KEY, ChatMode, set_ironcrew_mode};
use crate::lua::crew_userdata::LuaCrew;
use crate::lua::loader::ProjectLoader;
use crate::tools::ToolCallContext;

fn write(root: &Path, relative: &str, source: &str) {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, source).unwrap();
}

fn source_a(root: &Path) {
    write(
        root,
        "crew.lua",
        "local v = require('root_value')\nCrew.new({goal='entry-a:' .. v, provider='openai', api_key='test'})",
    );
    write(root, "config.lua", "return { model = 'model-a' }");
    write(
        root,
        "agents/assistant.lua",
        "return { name='assistant', goal='agent-a' }",
    );
    write(
        root,
        "tools/snapshot_tool.lua",
        "return { name='snapshot_tool', description='snapshot test', parameters={}, execute=function() return 'tool-a' end }",
    );
    write(root, "_lib/root_value.lua", "return 'require-a'");
    write(
        root,
        "nested/child.lua",
        "return 'child-a:' .. run_flow('inner.lua', {})",
    );
    write(root, "nested/inner.lua", "return require('child_value')");
    write(
        root,
        "nested/agents/helper.lua",
        "return { name='helper', goal='nested-agent-a' }",
    );
    write(
        root,
        "nested/_lib/child_value.lua",
        "return 'nested-require-a'",
    );
}

fn source_b(root: &Path) {
    for relative in [
        "crew.lua",
        "config.lua",
        "agents/assistant.lua",
        "tools/snapshot_tool.lua",
        "_lib/root_value.lua",
        "nested/child.lua",
        "nested/inner.lua",
        "nested/agents/helper.lua",
        "nested/_lib/child_value.lua",
    ] {
        write(root, relative, "this is invalid replacement Lua !!!");
    }
    write(
        root,
        "agents/late.lua",
        "this late role must not be discovered !!!",
    );
    write(
        root,
        "tools/late.lua",
        "this late role must not be discovered !!!",
    );
}

#[cfg(unix)]
#[tokio::test]
async fn http_runtime_executes_one_snapshot_across_source_aba() {
    let directory = TempDir::new().unwrap();
    source_a(directory.path());
    let snapshot = Arc::new(capture_flow_source(directory.path()).unwrap());
    let fingerprint_a = snapshot.fingerprint().to_owned();

    // Replace every executable role after capture. Any path-based reread now
    // fails parsing, while the immutable A snapshot remains executable.
    source_b(directory.path());
    let loader = ProjectLoader::from_conversation_snapshot(&snapshot).unwrap();
    let (lua, runtime, entrypoint) =
        setup_http_conversation_runtime(&loader, snapshot.clone()).unwrap();
    lua.set_app_data(ChatMode);
    set_ironcrew_mode(&lua, "chat").unwrap();
    lua.load(entrypoint.source()).exec_async().await.unwrap();
    lua.remove_app_data::<crate::lua::bootstrap::HttpConversationBootstrap>();

    let crew_userdata: AnyUserData = lua.named_registry_value(CHAT_CREW_REGISTRY_KEY).unwrap();
    let crew = crew_userdata.borrow::<LuaCrew>().unwrap().crew.clone();
    let crew = crew.lock().await;
    assert_eq!(crew.goal, "entry-a:require-a");
    assert_eq!(crew.provider_config.model, "model-a");
    assert_eq!(crew.agents.len(), 1);
    assert_eq!(crew.agents[0].goal, "agent-a");
    drop(crew);

    let tool = runtime
        .tool_registry
        .execute(
            "snapshot_tool",
            serde_json::json!({}),
            &ToolCallContext::default(),
        )
        .await
        .unwrap();
    assert_eq!(tool, "tool-a");

    let nested: String = lua
        .load("return run_flow('nested/child.lua', {})")
        .eval_async()
        .await
        .unwrap();
    assert_eq!(nested, "child-a:nested-require-a");

    // Complete the A -> B -> A cycle. The tree identity returns to A, but all
    // execution above was already tied to the original captured bytes.
    fs::remove_file(directory.path().join("agents/late.lua")).unwrap();
    fs::remove_file(directory.path().join("tools/late.lua")).unwrap();
    source_a(directory.path());
    assert_eq!(
        capture_flow_source(directory.path()).unwrap().fingerprint(),
        fingerprint_a
    );
}
