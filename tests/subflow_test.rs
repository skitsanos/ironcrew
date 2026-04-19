//! Integration tests for the `run_flow` Lua primitive and the shared
//! `invoke_subflow` implementation. All tests are hermetic — no LLM calls,
//! no network I/O, no subprocess spawning. The fake provider at the bottom
//! of this file short-circuits any accidental `crew:run()` invocation.

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use ironcrew::engine::runtime::Runtime;
use ironcrew::llm::provider::{ChatRequest, ChatResponse, LlmProvider, TokenUsage, ToolSchema};
use ironcrew::lua::api::{
    load_agents_from_files, load_tool_defs_from_files, register_agent_constructor,
    register_crew_constructor,
};
use ironcrew::lua::loader::ProjectLoader;
use ironcrew::lua::sandbox::create_crew_lua;
use ironcrew::lua::subflow::SubflowDepth;
use ironcrew::tools::ToolCallContext;
use ironcrew::utils::error::IronCrewError;
use mlua::Value;

/// A no-op LLM provider used as a placeholder in the fixture `Runtime`.
/// If a test accidentally calls `crew:run()`, the error surfaces loudly
/// rather than making a real API call.
struct NoopProvider;

#[async_trait]
impl LlmProvider for NoopProvider {
    async fn chat(&self, _request: ChatRequest) -> ironcrew::utils::error::Result<ChatResponse> {
        Err(IronCrewError::Provider("NoopProvider: no LLM calls".into()))
    }

    async fn chat_with_tools(
        &self,
        _request: ChatRequest,
        _tools: &[ToolSchema],
    ) -> ironcrew::utils::error::Result<ChatResponse> {
        Err(IronCrewError::Provider("NoopProvider: no LLM calls".into()))
    }
}

/// Suppress a dead_code lint on TokenUsage which is imported through the
/// provider prelude but not referenced directly in tests.
#[allow(dead_code)]
fn _unused_token_usage() -> TokenUsage {
    TokenUsage::default()
}

/// Build a top-level Lua VM and `Arc<Runtime>` for a given project
/// directory. Mirrors what `setup_crew_runtime` does, but uses
/// `NoopProvider` so no LLM calls can slip through.
fn build_fixture_lua(project_dir: &Path) -> (mlua::Lua, Arc<Runtime>) {
    let lua = create_crew_lua().expect("create_crew_lua");
    register_agent_constructor(&lua).expect("register_agent_constructor");

    let provider = Box::new(NoopProvider);
    let mut runtime = Runtime::new(provider, Some(project_dir));

    // Load the project's Lua tools (if any) and register them on the
    // runtime so the weak self-ref propagates to them when we upgrade the
    // Arc below.
    if let Ok(loader) = ProjectLoader::from_directory(project_dir) {
        let tool_defs = load_tool_defs_from_files(loader.tool_files()).expect("tool defs");
        runtime
            .register_lua_tools(tool_defs)
            .expect("register lua tools");
        let preloaded_agents = load_agents_from_files(loader.agent_files()).expect("agents");
        let runtime = Arc::new(runtime);
        runtime.set_self_ref(Arc::downgrade(&runtime));

        // Seed app-data on the top-level VM so `run_flow` works from it.
        let project_dir_arc = Arc::new(project_dir.to_path_buf());
        lua.set_app_data(runtime.clone());
        lua.set_app_data(project_dir_arc);
        lua.set_app_data(SubflowDepth(0));

        register_crew_constructor(
            &lua,
            runtime.clone(),
            preloaded_agents,
            project_dir.to_path_buf(),
        )
        .expect("register_crew_constructor");

        return (lua, runtime);
    }

    // Non-project (single-file) path — bind empty agents/tools.
    let runtime = Arc::new(runtime);
    runtime.set_self_ref(Arc::downgrade(&runtime));
    let project_dir_arc = Arc::new(project_dir.to_path_buf());
    lua.set_app_data(runtime.clone());
    lua.set_app_data(project_dir_arc);
    lua.set_app_data(SubflowDepth(0));
    register_crew_constructor(&lua, runtime.clone(), Vec::new(), project_dir.to_path_buf())
        .expect("register_crew_constructor");
    (lua, runtime)
}

// ──────────────────────────────────────────────────────────────────────────
// Path traversal rejection
// ──────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn run_flow_rejects_absolute_path() {
    let dir = tempfile::tempdir().unwrap();
    let (lua, _runtime) = build_fixture_lua(dir.path());

    let script = r#"
        local ok, err = pcall(function() return run_flow("/etc/passwd", {}) end)
        if ok then return "OK" else return tostring(err) end
    "#;
    let result: String = lua.load(script).eval_async().await.expect("eval");
    assert!(
        result.contains("Invalid subworkflow path"),
        "expected rejection, got: {}",
        result
    );
}

#[tokio::test]
async fn run_flow_rejects_parent_dir() {
    let dir = tempfile::tempdir().unwrap();
    let (lua, _runtime) = build_fixture_lua(dir.path());

    let script = r#"
        local ok, err = pcall(function() return run_flow("../escape.lua", {}) end)
        if ok then return "OK" else return tostring(err) end
    "#;
    let result: String = lua.load(script).eval_async().await.expect("eval");
    assert!(
        result.contains("Invalid subworkflow path"),
        "expected rejection, got: {}",
        result
    );
}

#[tokio::test]
async fn run_flow_rejects_empty_path() {
    let dir = tempfile::tempdir().unwrap();
    let (lua, _runtime) = build_fixture_lua(dir.path());

    let script = r#"
        local ok, err = pcall(function() return run_flow("", {}) end)
        if ok then return "OK" else return tostring(err) end
    "#;
    let result: String = lua.load(script).eval_async().await.expect("eval");
    assert!(
        result.contains("Invalid subworkflow path"),
        "expected rejection, got: {}",
        result
    );
}

#[tokio::test]
async fn run_flow_rejects_symlink_escape() {
    let dir = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let outside_file = outside.path().join("escape.lua");
    std::fs::write(&outside_file, "return { escaped = true }").unwrap();

    // Create a symlink inside the project dir that points outside it.
    let link = dir.path().join("escape.lua");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&outside_file, &link).unwrap();
    #[cfg(windows)]
    std::os::windows::fs::symlink_file(&outside_file, &link).unwrap();

    let (lua, _runtime) = build_fixture_lua(dir.path());
    let script = r#"
        local ok, err = pcall(function() return run_flow("escape.lua", {}) end)
        if ok then return "OK" else return tostring(err) end
    "#;
    let result: String = lua.load(script).eval_async().await.expect("eval");
    assert!(
        result.contains("escapes project directory"),
        "expected escape rejection, got: {}",
        result
    );
}

// ──────────────────────────────────────────────────────────────────────────
// Depth cap
// ──────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn run_flow_depth_cap_enforced() {
    // Build a chain: a.lua -> b.lua -> c.lua (depth 0 -> 1 -> 2 -> 3).
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.lua"), r#"return run_flow("b.lua", {})"#).unwrap();
    std::fs::write(dir.path().join("b.lua"), r#"return run_flow("c.lua", {})"#).unwrap();
    std::fs::write(dir.path().join("c.lua"), r#"return run_flow("d.lua", {})"#).unwrap();
    std::fs::write(dir.path().join("d.lua"), r#"return { ok = true }"#).unwrap();

    // Scope the env var override to this test. Using SetOnce-ish idiom
    // since std::env is process-global; test is single-threaded here.
    unsafe {
        std::env::set_var("IRONCREW_MAX_FLOW_DEPTH", "2");
    }
    let (lua, _runtime) = build_fixture_lua(dir.path());

    let script = r#"
        local ok, err = pcall(function() return run_flow("a.lua", {}) end)
        if ok then return "OK" else return tostring(err) end
    "#;
    let result: String = lua.load(script).eval_async().await.expect("eval");
    unsafe {
        std::env::remove_var("IRONCREW_MAX_FLOW_DEPTH");
    }

    assert!(
        result.contains("depth exceeded"),
        "expected depth-exceeded error, got: {}",
        result
    );
}

// ──────────────────────────────────────────────────────────────────────────
// Happy-path round trip
// ──────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn run_flow_top_level_happy_path() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("sub.lua"),
        r#"return { got = (input and input.x or 0) + 1 }"#,
    )
    .unwrap();

    let (lua, _runtime) = build_fixture_lua(dir.path());

    let script = r#"
        local result = run_flow("sub.lua", { x = 1 })
        return result.got
    "#;
    let got: i64 = lua.load(script).eval_async().await.expect("eval");
    assert_eq!(got, 2);
}

// ──────────────────────────────────────────────────────────────────────────
// Resolution relative to caller VM's dir (not parent's)
// ──────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn run_flow_resolves_against_caller_dir() {
    let project = tempfile::tempdir().unwrap();
    let subs_dir = project.path().join("subs");
    std::fs::create_dir(&subs_dir).unwrap();

    // subs/outer.lua dispatches to subs/inner.lua using a relative name
    // with NO prefix — if it ever resolved against the top-level project
    // dir, it would look for `inner.lua` at the root instead of the subs/
    // dir and fail.
    std::fs::write(
        subs_dir.join("outer.lua"),
        r#"return run_flow("inner.lua", {})"#,
    )
    .unwrap();
    std::fs::write(subs_dir.join("inner.lua"), r#"return { from = "inner" }"#).unwrap();

    let (lua, _runtime) = build_fixture_lua(project.path());
    let script = r#"
        local result = run_flow("subs/outer.lua", {})
        return result.from
    "#;
    let from: String = lua.load(script).eval_async().await.expect("eval");
    assert_eq!(from, "inner");
}

// ──────────────────────────────────────────────────────────────────────────
// Missing app-data -> clean error
// ──────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn run_flow_without_runtime_fails_clean() {
    // A bare VM — `create_crew_lua` installs `run_flow` via
    // `register_lua_globals` but we deliberately do NOT seed the app-data.
    let lua = create_crew_lua().expect("create_crew_lua");

    let script = r#"
        local ok, err = pcall(function() return run_flow("x.lua", {}) end)
        if ok then return "OK" else return tostring(err) end
    "#;
    let result: String = lua.load(script).eval_async().await.expect("eval");
    assert!(
        result.contains("no Runtime bound") || result.contains("no project_dir bound"),
        "expected clean validation error, got: {}",
        result
    );
}

// ──────────────────────────────────────────────────────────────────────────
// Full stack: LuaScriptTool invokes run_flow through tool_registry.execute
// ──────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn lua_tool_can_invoke_run_flow() {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/subflow_stub");

    // Build the fixture runtime. `build_fixture_lua` wires up
    // register_lua_tools + set_self_ref so the LuaScriptTool receives the
    // weak runtime reference and can seed its sandbox VM properly.
    let (_lua, runtime) = build_fixture_lua(&fixture);

    // Re-enter the same tool registry from outside Lua — simulates what
    // `ToolRegistry::execute` does during a real `crew:run()` loop.
    let ctx = ToolCallContext::default();
    let out = runtime
        .tool_registry
        .execute("delegator", serde_json::json!({ "x": 41 }), &ctx)
        .await
        .expect("delegator execute");

    assert_eq!(out, "got=42");
}

// ──────────────────────────────────────────────────────────────────────────
// Re-entrant: nested run_flow through a tool must reuse the same registry
// without deadlocking or double-borrowing the Arc.
// ──────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn nested_run_flow_through_tool_reuses_registry() {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/subflow_stub");
    let (_lua, runtime) = build_fixture_lua(&fixture);

    // Call the delegator tool twice concurrently. Exercises the Arc
    // registry + weak self-ref under contention.
    let r1 = runtime.clone();
    let r2 = runtime.clone();
    let h1 = tokio::spawn(async move {
        let ctx = ToolCallContext::default();
        r1.tool_registry
            .execute("delegator", serde_json::json!({ "x": 1 }), &ctx)
            .await
    });
    let h2 = tokio::spawn(async move {
        let ctx = ToolCallContext::default();
        r2.tool_registry
            .execute("delegator", serde_json::json!({ "x": 10 }), &ctx)
            .await
    });

    let out1 = h1.await.unwrap().expect("first delegator call");
    let out2 = h2.await.unwrap().expect("second delegator call");
    assert_eq!(out1, "got=2");
    assert_eq!(out2, "got=11");
}

// ──────────────────────────────────────────────────────────────────────────
// Regression: legacy crew:subworkflow still works after the extraction.
// ──────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn legacy_subworkflow_still_works() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("sub.lua"),
        r#"return { doubled = (input and input.n or 0) * 2 }"#,
    )
    .unwrap();

    let (lua, _runtime) = build_fixture_lua(dir.path());

    // Use crew:subworkflow through the Lua API.
    let script = r#"
        local crew = Crew.new({
            goal = "legacy test",
            provider = "openai",
            model = "test",
            api_key = "test",
        })
        local result = crew:subworkflow("sub.lua", { input = { n = 21 } })
        return result.doubled
    "#;
    let val: Value = lua.load(script).eval_async().await.expect("eval");
    let doubled: i64 = match val {
        Value::Integer(i) => i,
        other => panic!("expected integer, got {:?}", other),
    };
    assert_eq!(doubled, 42);
}
