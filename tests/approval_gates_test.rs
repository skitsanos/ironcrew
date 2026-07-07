//! Approval gates end-to-end through the crew:run chain: an agent calls a
//! gated tool, the run suspends on an approval question (kind: "approval"),
//! and the human's verdict decides whether the tool executes. Scripted
//! provider — no LLM, no network.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use ironcrew::engine::input_bridge::{AskHumanContext, BridgeMode, InputBridge};
use ironcrew::engine::runtime::Runtime;
use ironcrew::llm::provider::{
    ChatRequest, ChatResponse, LlmProvider, ToolCallFunction, ToolCallRequest, ToolSchema,
};
use ironcrew::lua::api::register_crew_constructor;
use ironcrew::lua::sandbox::create_crew_lua;
use ironcrew::utils::error::Result;

/// Scripted model: issues `tool_rounds` template_render calls (one per
/// turn), then returns `FINAL:<joined tool results>`.
struct GatedToolProvider {
    calls: AtomicUsize,
    tool_rounds: usize,
}

#[async_trait]
impl LlmProvider for GatedToolProvider {
    async fn chat(&self, _request: ChatRequest) -> Result<ChatResponse> {
        Ok(ChatResponse {
            content: Some("no tools".into()),
            ..Default::default()
        })
    }

    async fn chat_with_tools(
        &self,
        request: ChatRequest,
        _tools: &[ToolSchema],
    ) -> Result<ChatResponse> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        if n < self.tool_rounds {
            Ok(ChatResponse {
                content: None,
                tool_calls: vec![ToolCallRequest {
                    id: format!("call-{}", n),
                    call_type: "function".into(),
                    function: ToolCallFunction {
                        name: "template_render".into(),
                        // Contains a sensitive-looking key so the redaction
                        // path is exercised in the approval prompt.
                        arguments: r#"{"template": "round {{ n }}", "data": {"n": 1, "api_key": "sk-secret-999"}}"#.into(),
                    },
                }],
                ..Default::default()
            })
        } else {
            let joined: Vec<String> = request
                .messages
                .iter()
                .filter(|m| m.role == "tool")
                .filter_map(|m| m.content.clone())
                .collect();
            Ok(ChatResponse {
                content: Some(format!("FINAL:{}", joined.join("|"))),
                ..Default::default()
            })
        }
    }
}

fn fixture(
    project_dir: &Path,
    tool_rounds: usize,
    with_bridge: bool,
) -> (mlua::Lua, Arc<InputBridge>) {
    let lua = create_crew_lua().expect("create_crew_lua");
    let provider = Box::new(GatedToolProvider {
        calls: AtomicUsize::new(0),
        tool_rounds,
    });
    let runtime = Arc::new(Runtime::new(provider, Some(project_dir)));
    runtime.set_self_ref(Arc::downgrade(&runtime));
    lua.set_app_data(runtime.clone());
    lua.set_app_data(Arc::new(project_dir.to_path_buf()));
    register_crew_constructor(&lua, runtime, Vec::new(), project_dir.to_path_buf()).unwrap();

    let bridge = Arc::new(InputBridge::new(BridgeMode::Http));
    if with_bridge {
        lua.set_app_data(AskHumanContext {
            bridge: bridge.clone(),
            run_id: None,
            store: None,
            eventbus: None,
        });
    }
    (lua, bridge)
}

fn flow(tool_rounds_unused: usize) -> String {
    let _ = tool_rounds_unused;
    r#"
    local crew = Crew.new({
        goal = "approval gates test",
        provider = "openai",
        model = "scripted",
        require_approval = { "template_render" },
    })
    crew:add_agent({
        name = "worker",
        goal = "render templates",
        tools = { "template_render" },
    })
    crew:add_task({
        name = "render",
        description = "Render",
        expected_output = "text",
        timeout_secs = 2,
    })
    local results = crew:run()
    return results[1].output
    "#
    .to_string()
}

/// Wait until an approval question is pending, assert its shape, return id.
async fn wait_for_approval(bridge: &InputBridge) -> String {
    for _ in 0..600 {
        if let Some(q) = bridge.list().first() {
            assert_eq!(q.kind, "approval", "gate questions carry kind=approval");
            assert!(
                q.prompt
                    .starts_with("[approval] Agent 'worker' wants to call template_render("),
                "got prompt: {}",
                q.prompt
            );
            // Redaction: the sensitive value must not reach the prompt.
            assert!(
                !q.prompt.contains("sk-secret-999"),
                "sensitive arg leaked into approval prompt: {}",
                q.prompt
            );
            assert_eq!(
                q.choices,
                vec![
                    "allow".to_string(),
                    "always".to_string(),
                    "deny".to_string()
                ]
            );
            return q.question_id.clone();
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("approval question never appeared");
}

#[tokio::test]
async fn allow_runs_the_tool_and_result_reaches_model() {
    let dir = tempfile::tempdir().unwrap();
    let (lua, bridge) = fixture(dir.path(), 1, true);
    let src = flow(1);
    let run = tokio::spawn(async move { lua.load(src).eval_async::<String>().await });

    let qid = wait_for_approval(&bridge).await;
    bridge.answer(&qid, serde_json::json!("allow")).unwrap();

    let output = run.await.unwrap().expect("flow should succeed");
    assert_eq!(output, "FINAL:round 1", "tool ran and result reached model");
}

#[tokio::test]
async fn deny_returns_operator_error_to_model() {
    let dir = tempfile::tempdir().unwrap();
    let (lua, bridge) = fixture(dir.path(), 1, true);
    let src = flow(1);
    let run = tokio::spawn(async move { lua.load(src).eval_async::<String>().await });

    let qid = wait_for_approval(&bridge).await;
    bridge.answer(&qid, serde_json::json!("deny")).unwrap();

    let output = run.await.unwrap().expect("flow completes; the TOOL failed");
    assert!(
        output.contains("denied by human operator"),
        "model must see the denial: {output}"
    );
}

#[tokio::test]
async fn free_text_denial_carries_reason_as_steering() {
    let dir = tempfile::tempdir().unwrap();
    let (lua, bridge) = fixture(dir.path(), 1, true);
    let src = flow(1);
    let run = tokio::spawn(async move { lua.load(src).eval_async::<String>().await });

    let qid = wait_for_approval(&bridge).await;
    bridge
        .answer(&qid, serde_json::json!("use the cached copy instead"))
        .unwrap();

    let output = run.await.unwrap().unwrap();
    assert!(
        output.contains("use the cached copy instead"),
        "denial text must reach the model as steering: {output}"
    );
}

#[tokio::test]
async fn always_grants_for_the_rest_of_the_run() {
    let dir = tempfile::tempdir().unwrap();
    // Two gated calls; only the FIRST should ask.
    let (lua, bridge) = fixture(dir.path(), 2, true);
    let src = flow(2);
    let run = tokio::spawn(async move { lua.load(src).eval_async::<String>().await });

    let qid = wait_for_approval(&bridge).await;
    bridge.answer(&qid, serde_json::json!("always")).unwrap();

    // The second call must pass without a new question.
    let output = run.await.unwrap().expect("flow should succeed");
    assert_eq!(output, "FINAL:round 1|round 1");
    assert_eq!(
        bridge.pending_count(),
        0,
        "no second approval question after 'always'"
    );
}

#[tokio::test]
async fn no_bridge_fails_closed() {
    let dir = tempfile::tempdir().unwrap();
    let (lua, _bridge) = fixture(dir.path(), 1, false); // no AskHumanContext
    let src = flow(1);

    let output = lua
        .load(src)
        .eval_async::<String>()
        .await
        .expect("flow completes; the TOOL was denied");
    assert!(
        output.contains("requires human approval") || output.contains("no approval channel"),
        "gate must fail closed without a bridge: {output}"
    );
}

#[tokio::test]
async fn ungated_tools_run_without_questions() {
    let dir = tempfile::tempdir().unwrap();
    let (lua, bridge) = fixture(dir.path(), 1, true);
    // Same flow but nothing gated: require_approval covers a different tool.
    let src = flow(1).replace(
        r#"require_approval = { "template_render" }"#,
        r#"require_approval = { "file_write" }"#,
    );
    let output = lua
        .load(src)
        .eval_async::<String>()
        .await
        .expect("flow should succeed with no gate");
    assert_eq!(output, "FINAL:round 1");
    assert_eq!(bridge.pending_count(), 0, "no questions for ungated tools");
}
