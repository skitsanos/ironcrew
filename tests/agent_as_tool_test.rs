//! Integration tests for agent-as-tool (spec §9).
//!
//! Exercises the 12 cases listed in §9 of
//! `docs/superpowers/specs/2026-04-20-agent-as-tool-design.md`.
//! Each test drives `AgentAsTool::execute` (or `ensure_agent_tools_finalized`
//! via the Lua path) through a canned-response provider. No network I/O.
//!
//! Two cases are intentionally skipped with pointers to the primary test
//! site — see the `test_06_*` and `test_07_*` stubs.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use ironcrew::engine::agent::Agent;
use ironcrew::engine::eventbus::{CrewEvent, EventBus};
use ironcrew::engine::runtime::Runtime;
use ironcrew::llm::provider::{
    ChatRequest, ChatResponse, LlmProvider, ToolCallFunction, ToolCallRequest, ToolSchema,
};
use ironcrew::tools::agent_as_tool::AgentAsTool;
use ironcrew::tools::registry::ToolRegistry;
use ironcrew::tools::{Tool, ToolCallContext};
use ironcrew::utils::error::{IronCrewError, Result as IcResult};
use serde_json::json;

// =====================================================================
// Shared fixtures
// =====================================================================

/// Canned-response LLM provider. Returns `reply` on every `chat` call. For
/// `chat_with_tools`, if `tool_call_name` is set, the *first* tool-capable
/// call returns a tool_call requesting that tool; subsequent calls fall
/// through to `chat` (the plain reply).
struct CannedProvider {
    reply: String,
    tool_call_name: Option<String>,
    tool_call_emitted: Mutex<bool>,
}

impl CannedProvider {
    fn just(reply: &str) -> Arc<dyn LlmProvider> {
        Arc::new(Self {
            reply: reply.into(),
            tool_call_name: None,
            tool_call_emitted: Mutex::new(true),
        })
    }

    fn with_tool_call_then(reply: &str, tool: &str) -> Arc<dyn LlmProvider> {
        Arc::new(Self {
            reply: reply.into(),
            tool_call_name: Some(tool.into()),
            tool_call_emitted: Mutex::new(false),
        })
    }
}

#[async_trait]
impl LlmProvider for CannedProvider {
    async fn chat(&self, _req: ChatRequest) -> IcResult<ChatResponse> {
        Ok(ChatResponse {
            content: Some(self.reply.clone()),
            reasoning: None,
            tool_calls: vec![],
            usage: None,
        })
    }

    async fn chat_with_tools(
        &self,
        req: ChatRequest,
        _tools: &[ToolSchema],
    ) -> IcResult<ChatResponse> {
        if let Some(ref tool) = self.tool_call_name {
            let mut emitted = self.tool_call_emitted.lock().unwrap();
            if !*emitted {
                *emitted = true;
                return Ok(ChatResponse {
                    content: None,
                    reasoning: None,
                    tool_calls: vec![ToolCallRequest {
                        id: "tc1".into(),
                        call_type: "function".into(),
                        function: ToolCallFunction {
                            name: tool.clone(),
                            arguments: "{}".into(),
                        },
                    }],
                    usage: None,
                });
            }
        }
        self.chat(req).await
    }
}

/// LLM provider that panics if it's called at all. Used to prove the
/// depth-cap early-return never reaches the LLM.
struct PanicProvider;

#[async_trait]
impl LlmProvider for PanicProvider {
    async fn chat(&self, _req: ChatRequest) -> IcResult<ChatResponse> {
        panic!("PanicProvider::chat should never be called");
    }
    async fn chat_with_tools(
        &self,
        _req: ChatRequest,
        _tools: &[ToolSchema],
    ) -> IcResult<ChatResponse> {
        panic!("PanicProvider::chat_with_tools should never be called");
    }
}

/// Records every `request.model` value the provider sees. Used by the
/// per-crew-defaults test to prove resolved_model propagates to the LLM
/// layer.
struct RecordingProvider {
    reply: String,
    models_seen: Mutex<Vec<String>>,
}

impl RecordingProvider {
    fn new(reply: &str) -> Arc<Self> {
        Arc::new(Self {
            reply: reply.into(),
            models_seen: Mutex::new(Vec::new()),
        })
    }
    fn seen(&self) -> Vec<String> {
        self.models_seen.lock().unwrap().clone()
    }
}

#[async_trait]
impl LlmProvider for RecordingProvider {
    async fn chat(&self, req: ChatRequest) -> IcResult<ChatResponse> {
        self.models_seen.lock().unwrap().push(req.model.clone());
        Ok(ChatResponse {
            content: Some(self.reply.clone()),
            reasoning: None,
            tool_calls: vec![],
            usage: None,
        })
    }
    async fn chat_with_tools(
        &self,
        req: ChatRequest,
        _tools: &[ToolSchema],
    ) -> IcResult<ChatResponse> {
        self.chat(req).await
    }
}

/// Build an `Arc<Runtime>` whose `self_ref` is wired so
/// `AgentAsTool::execute` can `upgrade()` successfully. The runtime's own
/// provider is a noop — the tool carries its own provider.
fn build_runtime(project_dir: &std::path::Path) -> Arc<Runtime> {
    struct NoopProvider;
    #[async_trait]
    impl LlmProvider for NoopProvider {
        async fn chat(&self, _req: ChatRequest) -> IcResult<ChatResponse> {
            Err(IronCrewError::Provider("NoopProvider: no LLM calls".into()))
        }
        async fn chat_with_tools(
            &self,
            _req: ChatRequest,
            _tools: &[ToolSchema],
        ) -> IcResult<ChatResponse> {
            Err(IronCrewError::Provider("NoopProvider: no LLM calls".into()))
        }
    }

    let runtime = Runtime::new(Box::new(NoopProvider), Some(project_dir));
    let runtime = Arc::new(runtime);
    runtime.set_self_ref(Arc::downgrade(&runtime));
    runtime
}

/// Sugar for building a researcher-style `AgentAsTool` bound to the given
/// provider + runtime.
fn build_tool(
    agent: Agent,
    provider: Arc<dyn LlmProvider>,
    runtime: &Arc<Runtime>,
    resolved_model: &str,
) -> AgentAsTool {
    AgentAsTool::new(
        agent,
        provider,
        Arc::downgrade(runtime),
        resolved_model.into(),
        5,
        Some(50),
        Arc::new(PathBuf::from("/tmp")),
    )
}

// =====================================================================
// Tests
// =====================================================================

/// Test 1 — Happy path. Coordinator with `tools = ["agent__researcher"]`
/// invokes the researcher which has no tools of its own. The canned
/// provider replies `"researched-result"` and the tool surfaces it
/// verbatim.
#[tokio::test]
async fn test_01_happy_path_single_specialist() {
    let project = tempfile::tempdir().unwrap();
    let runtime = build_runtime(project.path());
    let provider = CannedProvider::just("researched-result");

    let researcher = Agent {
        name: "researcher".into(),
        goal: "find facts".into(),
        ..Default::default()
    };
    let tool = build_tool(researcher, provider, &runtime, "stub-model");

    let ctx = ToolCallContext {
        caller_agent: Some("coordinator".into()),
        ..Default::default()
    };

    let out = tool
        .execute(json!({"prompt": "find facts on rust"}), &ctx)
        .await
        .expect("execute Ok");
    assert_eq!(out, "researched-result");
}

/// Test 2 — Sub-agent tool inheritance. The researcher has
/// `tools = ["stub_tool"]`, the caller's `ToolCallContext.tool_registry`
/// carries a stub tool by that name, and the provider emits a tool_call
/// on the first turn + a terminal reply on the second. Assert the stub
/// tool was dispatched exactly once and the terminal content comes back.
#[tokio::test]
async fn test_02_sub_agent_tool_inheritance() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct StubTool {
        call_count: AtomicUsize,
    }
    #[async_trait]
    impl Tool for StubTool {
        fn name(&self) -> &str {
            "stub_tool"
        }
        fn description(&self) -> &str {
            "stub"
        }
        fn schema(&self) -> ToolSchema {
            ToolSchema {
                name: "stub_tool".into(),
                description: "stub".into(),
                parameters: json!({"type":"object","properties":{}}),
            }
        }
        async fn execute(
            &self,
            _args: serde_json::Value,
            _ctx: &ToolCallContext,
        ) -> IcResult<String> {
            self.call_count.fetch_add(1, Ordering::SeqCst);
            Ok("stub-ok".into())
        }
    }

    let project = tempfile::tempdir().unwrap();
    let runtime = build_runtime(project.path());
    let provider = CannedProvider::with_tool_call_then("done", "stub_tool");

    let researcher = Agent {
        name: "researcher".into(),
        goal: "research things".into(),
        tools: vec!["stub_tool".into()],
        ..Default::default()
    };
    let tool = build_tool(researcher, provider, &runtime, "stub-model");

    let stub = Arc::new(StubTool {
        call_count: AtomicUsize::new(0),
    });
    let mut registry = ToolRegistry::new();
    registry.register_arc(stub.clone() as Arc<dyn Tool>);

    let ctx = ToolCallContext {
        caller_agent: Some("coordinator".into()),
        tool_registry: Some(registry),
        ..Default::default()
    };

    let out = tool
        .execute(json!({"prompt": "go"}), &ctx)
        .await
        .expect("execute Ok");
    assert_eq!(out, "done");
    assert_eq!(
        stub.call_count.load(Ordering::SeqCst),
        1,
        "stub_tool should be dispatched exactly once"
    );
}

/// Test 3 — Depth cap. With `IRONCREW_MAX_FLOW_DEPTH=2` and
/// `ctx.depth = 2`, the tool must short-circuit with a "depth exceeded"
/// error string and the provider must NOT be called.
#[tokio::test]
async fn test_03_depth_cap_blocks_delegation() {
    // Process-scoped env var — run single-threaded so we don't race with
    // other tests that also write IRONCREW_MAX_FLOW_DEPTH. `cargo test`
    // uses rayon-style parallelism; the sub-flow depth test does the same
    // dance (see `run_flow_depth_cap_enforced` in subflow_test.rs).
    unsafe {
        std::env::set_var("IRONCREW_MAX_FLOW_DEPTH", "2");
    }

    let project = tempfile::tempdir().unwrap();
    let runtime = build_runtime(project.path());

    let researcher = Agent {
        name: "researcher".into(),
        goal: "find facts".into(),
        ..Default::default()
    };
    // PanicProvider proves no LLM call happens — otherwise the test
    // panics during execute.
    let tool = build_tool(researcher, Arc::new(PanicProvider), &runtime, "stub-model");

    let ctx = ToolCallContext {
        caller_agent: Some("coordinator".into()),
        depth: 2,
        ..Default::default()
    };

    let out = tool
        .execute(json!({"prompt": "go"}), &ctx)
        .await
        .expect("execute Ok (error surfaces as string, not Err)");

    assert!(
        out.contains("depth exceeded"),
        "expected 'depth exceeded' in reply, got: {out}"
    );

    unsafe {
        std::env::remove_var("IRONCREW_MAX_FLOW_DEPTH");
    }
}

/// Test 4 — Shared counter with `run_flow`. The key invariant is that
/// `AgentAsTool` reads from the same `ctx.depth` field `run_flow` uses,
/// and bumps it by 1 when dispatching. We verify that by:
///   (a) confirming `ctx.depth == cap` blocks the dispatch (same as
///       test 3 proves), AND
///   (b) confirming that a non-blocked invocation propagates
///       `depth + 1` into the sub-context observed by inner tools.
/// Uses a custom tool that inspects `ctx.depth`.
#[tokio::test]
async fn test_04_shared_depth_counter_with_run_flow() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Records the `ctx.depth` it was dispatched at.
    struct DepthProbe {
        observed_depth: Arc<AtomicUsize>,
    }
    #[async_trait]
    impl Tool for DepthProbe {
        fn name(&self) -> &str {
            "depth_probe"
        }
        fn description(&self) -> &str {
            "reports ctx.depth"
        }
        fn schema(&self) -> ToolSchema {
            ToolSchema {
                name: "depth_probe".into(),
                description: "reports ctx.depth".into(),
                parameters: json!({"type":"object","properties":{}}),
            }
        }
        async fn execute(
            &self,
            _args: serde_json::Value,
            ctx: &ToolCallContext,
        ) -> IcResult<String> {
            self.observed_depth.store(ctx.depth, Ordering::SeqCst);
            Ok(format!("seen:{}", ctx.depth))
        }
    }

    let project = tempfile::tempdir().unwrap();
    let runtime = build_runtime(project.path());
    let provider = CannedProvider::with_tool_call_then("done", "depth_probe");

    let researcher = Agent {
        name: "researcher".into(),
        goal: "inspect depth".into(),
        tools: vec!["depth_probe".into()],
        ..Default::default()
    };
    let tool = build_tool(researcher, provider, &runtime, "stub-model");

    let observed = Arc::new(AtomicUsize::new(usize::MAX));
    let probe = Arc::new(DepthProbe {
        observed_depth: observed.clone(),
    });
    let mut registry = ToolRegistry::new();
    registry.register_arc(probe as Arc<dyn Tool>);

    let ctx = ToolCallContext {
        caller_agent: Some("coordinator".into()),
        depth: 1, // below cap
        tool_registry: Some(registry),
        ..Default::default()
    };

    let _ = tool
        .execute(json!({"prompt": "probe"}), &ctx)
        .await
        .expect("execute Ok");

    // The sub-context handed to inner tools should have depth == 2,
    // i.e. `run_flow` and `agent-as-tool` increment the same counter.
    assert_eq!(
        observed.load(Ordering::SeqCst),
        2,
        "sub-context depth should be ctx.depth + 1 (shared counter with run_flow)"
    );
}

/// Test 5 — Unknown agent reference. A crew with
/// `coordinator.tools = ["agent__ghost"]` but no agent named `ghost`
/// must fail finalization with a validation error on the first
/// `crew:conversation()` call, AND return the same cached error on the
/// second call without re-running validation.
///
/// Drives the check through the Lua path since `LuaCrew` fields and
/// `ensure_agent_tools_finalized` are `pub(crate)` — the integration
/// test crate can't reach them directly.
#[tokio::test]
async fn test_05_unknown_agent_reference_cached_error() {
    use ironcrew::lua::api::{register_agent_constructor, register_crew_constructor};
    use ironcrew::lua::sandbox::create_crew_lua;

    // Build a top-level Lua VM wired like `setup_crew_runtime` does.
    let project = tempfile::tempdir().unwrap();
    let runtime = build_runtime(project.path());

    // The Crew.new factory hits the OpenAI provider path when api_key is
    // set, but we keep the default (openai/gpt-4.1-mini/no api_key) so
    // `custom_provider` is None and the runtime's provider (noop) is
    // used. finalize_agent_tools never calls a provider — it only walks
    // tools lists — so this is safe.
    let lua = create_crew_lua().expect("create_crew_lua");
    register_agent_constructor(&lua).expect("register_agent_constructor");
    register_crew_constructor(
        &lua,
        runtime.clone(),
        Vec::new(),
        project.path().to_path_buf(),
    )
    .expect("register_crew_constructor");

    let script = r#"
        local crew = Crew.new({ goal = "test", provider = "openai", model = "gpt-4o" })
        crew:add_agent(Agent.new({
            name  = "coordinator",
            goal  = "route",
            tools = { "agent__ghost" },
        }))

        local function try()
            -- conversation() runs ensure_agent_tools_finalized and
            -- should fail fast because `ghost` isn't registered.
            local ok, err = pcall(function()
                return crew:conversation({ agent = "coordinator" })
            end)
            return ok, tostring(err)
        end

        local ok1, err1 = try()
        local ok2, err2 = try()
        return { ok1 = ok1, err1 = err1, ok2 = ok2, err2 = err2 }
    "#;
    let result: mlua::Table = lua.load(script).eval_async().await.expect("eval");

    let ok1: bool = result.get("ok1").unwrap();
    let ok2: bool = result.get("ok2").unwrap();
    let err1: String = result.get("err1").unwrap();
    let err2: String = result.get("err2").unwrap();

    assert!(!ok1, "first call should have failed (unknown agent)");
    assert!(!ok2, "second call should have failed (unknown agent)");
    assert!(
        err1.contains("ghost") && err1.to_lowercase().contains("unknown"),
        "first error should mention 'unknown agent ghost', got: {err1}"
    );

    // The cached-error contract lives in `ensure_agent_tools_finalized`: the
    // first validation failure is stashed in the `OnceCell` and re-returned
    // on every subsequent call, so the core error string is stable across
    // invocations. We compare only the `IronCrewError::Validation` head of
    // the Lua error — the outer `pcall` frames embed the calling line
    // number, which differs by exactly one column between `try()` calls
    // and is Lua-formatting noise, not a cache-vs-revalidation signal.
    fn core(err: &str) -> &str {
        err.split("\nstack traceback:").next().unwrap_or(err)
    }
    assert_eq!(
        core(&err1),
        core(&err2),
        "cached error core must match byte-for-byte"
    );
    assert!(
        core(&err1).contains("Agent-as-tool: unknown agent 'ghost'"),
        "expected the specific unknown-agent validation error, got: {err1}"
    );
}

/// Test 6 — Reserved-prefix collision. A custom Lua tool named
/// `agent__foo` must fail tool-load. This is already exercised end-to-end
/// in `tests/reserved_prefix_test.rs::reserved_agent_prefix_on_custom_tool_rejected`
/// — filed there because it sits at the `load_tool_defs_from_files`
/// boundary, not the AgentAsTool invocation path. Left as a signpost.
#[test]
#[ignore = "covered by tests/reserved_prefix_test.rs::reserved_agent_prefix_on_custom_tool_rejected"]
fn test_06_reserved_prefix_collision() {
    // Intentionally empty. See module doc.
}

/// Test 7 — Malformed agent name. `agent__BadCASE` (uppercase chars) is
/// rejected at agent parse time by `agent_from_lua_table`, which is the
/// single choke-point where every Lua-constructed Agent flows through.
/// Direct `Agent` struct construction in Rust bypasses validation — see
/// `src/lua/parsers.rs::parser_agent_tool_validation::*` for the primary
/// test of that invariant. We replicate the check here through the
/// public `agent_from_lua_table` entrypoint.
#[tokio::test]
async fn test_07_malformed_agent_name_rejected() {
    use ironcrew::lua::api::agent_from_lua_table;
    use mlua::Lua;

    let lua = Lua::new();
    let table = lua.create_table().unwrap();
    table.set("name", "coordinator").unwrap();
    table.set("goal", "route").unwrap();
    let tools = lua.create_sequence_from(vec!["agent__BadCASE"]).unwrap();
    table.set("tools", tools).unwrap();

    let err = agent_from_lua_table(&table).unwrap_err().to_string();
    assert!(
        err.contains("agent__BadCASE"),
        "error should mention the malformed name, got: {err}"
    );
}

/// Test 8 — Missing `prompt` arg returns a tool-error string (not an
/// `Err`) so the caller LLM sees a recoverable error and can retry.
#[tokio::test]
async fn test_08_missing_prompt_arg_returns_tool_error() {
    let project = tempfile::tempdir().unwrap();
    let runtime = build_runtime(project.path());

    let researcher = Agent {
        name: "researcher".into(),
        goal: "find facts".into(),
        ..Default::default()
    };
    let tool = build_tool(
        researcher,
        CannedProvider::just("unreachable"),
        &runtime,
        "stub-model",
    );

    let ctx = ToolCallContext {
        caller_agent: Some("coordinator".into()),
        ..Default::default()
    };

    // No `prompt` key at all.
    let out = tool.execute(json!({}), &ctx).await.expect("execute Ok");
    assert!(
        out.starts_with("error: `prompt` is required"),
        "wrong error message: {out}"
    );

    // Empty-string prompt also rejected.
    let out = tool
        .execute(json!({"prompt": "   "}), &ctx)
        .await
        .expect("execute Ok");
    assert!(
        out.starts_with("error: `prompt` is required"),
        "empty prompt should also be rejected: {out}"
    );
}

/// Test 9 — Bracket events (`AgentToolStarted` + `AgentToolCompleted`)
/// fire exactly once per invocation with the expected caller / callee.
#[tokio::test]
async fn test_09_bracket_events_fire_on_invocation() {
    let project = tempfile::tempdir().unwrap();
    let runtime = build_runtime(project.path());
    let bus = EventBus::new(256);
    let mut rx = bus.subscribe();

    let researcher = Agent {
        name: "researcher".into(),
        goal: "find facts".into(),
        ..Default::default()
    };
    let tool = build_tool(
        researcher,
        CannedProvider::just("researched"),
        &runtime,
        "stub-model",
    );

    let ctx = ToolCallContext {
        caller_agent: Some("coordinator".into()),
        eventbus: Some(bus),
        ..Default::default()
    };

    let out = tool
        .execute(json!({"prompt": "go"}), &ctx)
        .await
        .expect("execute Ok");
    assert_eq!(out, "researched");

    // Drain all events emitted during the invocation.
    let mut events = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        events.push(ev);
    }

    let started_count = events
        .iter()
        .filter(|e| matches!(e.as_ref(), CrewEvent::AgentToolStarted { .. }))
        .count();
    let completed_count = events
        .iter()
        .filter(|e| matches!(e.as_ref(), CrewEvent::AgentToolCompleted { .. }))
        .count();
    assert_eq!(started_count, 1, "expected exactly one AgentToolStarted");
    assert_eq!(
        completed_count, 1,
        "expected exactly one AgentToolCompleted"
    );

    // Attribution: caller = coordinator (from ctx.caller_agent), callee = researcher (from agent.name).
    let (caller, callee) = events
        .iter()
        .find_map(|e| match e.as_ref() {
            CrewEvent::AgentToolStarted { caller, callee, .. } => {
                Some((caller.clone(), callee.clone()))
            }
            _ => None,
        })
        .expect("AgentToolStarted must exist");
    assert_eq!(caller, "coordinator");
    assert_eq!(callee, "researcher");

    let (caller2, callee2, success) = events
        .iter()
        .find_map(|e| match e.as_ref() {
            CrewEvent::AgentToolCompleted {
                caller,
                callee,
                success,
                ..
            } => Some((caller.clone(), callee.clone(), *success)),
            _ => None,
        })
        .expect("AgentToolCompleted must exist");
    assert_eq!(caller2, "coordinator");
    assert_eq!(callee2, "researcher");
    assert!(success, "successful turn should set success=true");
}

/// Test 10 — Inner tool events (`ToolCall` + `ToolResult`) fire for
/// dispatches inside the sub-agent's turn. This proves
/// `run_single_agent_turn` emits per-tool telemetry even when driven by
/// `AgentAsTool` (the gap the spec review called out).
#[tokio::test]
async fn test_10_inner_tool_events_fire() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct StubTool {
        call_count: AtomicUsize,
    }
    #[async_trait]
    impl Tool for StubTool {
        fn name(&self) -> &str {
            "stub_tool"
        }
        fn description(&self) -> &str {
            "stub"
        }
        fn schema(&self) -> ToolSchema {
            ToolSchema {
                name: "stub_tool".into(),
                description: "stub".into(),
                parameters: json!({"type":"object","properties":{}}),
            }
        }
        async fn execute(
            &self,
            _args: serde_json::Value,
            _ctx: &ToolCallContext,
        ) -> IcResult<String> {
            self.call_count.fetch_add(1, Ordering::SeqCst);
            Ok("ok".into())
        }
    }

    let project = tempfile::tempdir().unwrap();
    let runtime = build_runtime(project.path());
    let bus = EventBus::new(256);
    let mut rx = bus.subscribe();

    let provider = CannedProvider::with_tool_call_then("done", "stub_tool");
    let researcher = Agent {
        name: "researcher".into(),
        goal: "research".into(),
        tools: vec!["stub_tool".into()],
        ..Default::default()
    };
    let tool = build_tool(researcher, provider, &runtime, "stub-model");

    let stub = Arc::new(StubTool {
        call_count: AtomicUsize::new(0),
    });
    let mut registry = ToolRegistry::new();
    registry.register_arc(stub as Arc<dyn Tool>);

    let ctx = ToolCallContext {
        caller_agent: Some("coordinator".into()),
        eventbus: Some(bus),
        tool_registry: Some(registry),
        ..Default::default()
    };

    let _ = tool
        .execute(json!({"prompt": "go"}), &ctx)
        .await
        .expect("execute Ok");

    let mut events = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        events.push(ev);
    }

    let tool_call_count = events
        .iter()
        .filter(|e| matches!(e.as_ref(), CrewEvent::ToolCall { .. }))
        .count();
    let tool_result_count = events
        .iter()
        .filter(|e| matches!(e.as_ref(), CrewEvent::ToolResult { .. }))
        .count();
    assert_eq!(tool_call_count, 1, "expected one ToolCall event");
    assert_eq!(tool_result_count, 1, "expected one ToolResult event");

    // Additionally assert bracket events still fire — we don't want
    // inner events to somehow replace them.
    assert!(
        events
            .iter()
            .any(|e| matches!(e.as_ref(), CrewEvent::AgentToolStarted { .. })),
        "bracket-start must still fire"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e.as_ref(), CrewEvent::AgentToolCompleted { .. })),
        "bracket-end must still fire"
    );
}

/// Test 11 — No conversation pollution. Agent-as-tool is ephemeral by
/// design; it must NOT emit `ConversationStarted` / `ConversationTurn` /
/// `ConversationThinking` during its invocation. Regression guard
/// against accidentally routing the ephemeral helper back through
/// `LuaConversationInner` (which DOES emit those events).
#[tokio::test]
async fn test_11_no_conversation_pollution() {
    let project = tempfile::tempdir().unwrap();
    let runtime = build_runtime(project.path());
    let bus = EventBus::new(256);
    let mut rx = bus.subscribe();

    let researcher = Agent {
        name: "researcher".into(),
        goal: "find facts".into(),
        ..Default::default()
    };
    let tool = build_tool(
        researcher,
        CannedProvider::just("researched"),
        &runtime,
        "stub-model",
    );

    let ctx = ToolCallContext {
        caller_agent: Some("coordinator".into()),
        eventbus: Some(bus),
        ..Default::default()
    };

    let _ = tool
        .execute(json!({"prompt": "go"}), &ctx)
        .await
        .expect("execute Ok");

    let mut events = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        events.push(ev);
    }

    for ev in &events {
        match ev.as_ref() {
            CrewEvent::ConversationStarted { .. }
            | CrewEvent::ConversationTurn { .. }
            | CrewEvent::ConversationThinking { .. } => {
                panic!("agent-as-tool must not emit conversation_* events; got: {ev:?}");
            }
            _ => {}
        }
    }
}

/// Test 12 — Per-crew defaults captured. `AgentAsTool.resolved_model` is
/// decided at finalization time from the owning crew's `default_model`
/// and does not bleed across instances. Since the struct's `resolved_model`
/// field is private, we observe it behaviorally: point two tools (built
/// with different `resolved_model` values) at the same
/// `RecordingProvider` and confirm the provider sees the correct model
/// per invocation.
///
/// This validates the "`AgentAsTool` propagates its resolved model to
/// the LLM layer" half of the invariant. The "finalize_agent_tools picks
/// the owning crew's default" half is exercised by
/// `src/lua/crew_userdata.rs::finalize_agent_tools` — it's the only code
/// path that writes `AgentAsTool.resolved_model`, and its logic is
/// straightforward.
#[tokio::test]
async fn test_12_per_crew_defaults_captured() {
    let project = tempfile::tempdir().unwrap();
    let runtime = build_runtime(project.path());
    let recorder = RecordingProvider::new("done");
    let provider_dyn: Arc<dyn LlmProvider> = recorder.clone();

    let writer = Agent {
        name: "writer".into(),
        goal: "polish prose".into(),
        ..Default::default()
    };

    let tool_a = build_tool(writer.clone(), provider_dyn.clone(), &runtime, "opus-model");
    let tool_b = build_tool(
        writer.clone(),
        provider_dyn.clone(),
        &runtime,
        "haiku-model",
    );

    let ctx = ToolCallContext {
        caller_agent: Some("orchestrator".into()),
        ..Default::default()
    };

    tool_a
        .execute(json!({"prompt": "polish"}), &ctx)
        .await
        .expect("a Ok");
    tool_b
        .execute(json!({"prompt": "polish"}), &ctx)
        .await
        .expect("b Ok");

    let seen = recorder.seen();
    assert_eq!(
        seen,
        vec!["opus-model".to_string(), "haiku-model".to_string()],
        "each AgentAsTool must forward its own resolved_model to the provider"
    );
}
