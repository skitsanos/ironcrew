//! Agent-initiated `ask_human`: an agent (LLM) decides mid-task to call the
//! `ask_human` TOOL, the whole `crew:run()` chain suspends on the per-run
//! bridge, a "human" answers, and the answer flows back into the model's
//! next turn. Exercises orchestrator → task_runner (pause-aware timeout) →
//! executor → registry → AskHumanTool with a scripted provider — no LLM,
//! no network.

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

/// Scripted model: on the first tool-enabled call it asks the human via the
/// `ask_human` tool; on the second call (tool result now in the history) it
/// answers with `FINAL:<the tool result>` so the test can assert the human's
/// answer reached the model verbatim.
struct AskingProvider {
    calls: AtomicUsize,
}

#[async_trait]
impl LlmProvider for AskingProvider {
    async fn chat(&self, _request: ChatRequest) -> Result<ChatResponse> {
        Ok(ChatResponse {
            content: Some("no tools offered".into()),
            ..Default::default()
        })
    }

    async fn chat_with_tools(
        &self,
        request: ChatRequest,
        tools: &[ToolSchema],
    ) -> Result<ChatResponse> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        if n == 0 {
            // The agent must actually be offered the tool it opted into.
            assert!(
                tools.iter().any(|t| t.name == "ask_human"),
                "ask_human schema not offered to the model; got: {:?}",
                tools.iter().map(|t| &t.name).collect::<Vec<_>>()
            );
            Ok(ChatResponse {
                content: None,
                tool_calls: vec![ToolCallRequest {
                    id: "call-ask-1".into(),
                    call_type: "function".into(),
                    function: ToolCallFunction {
                        name: "ask_human".into(),
                        arguments: r#"{"question": "Which quarter should I analyze?", "choices": ["Q1", "Q2"], "timeout_s": 30}"#.into(),
                    },
                }],
                ..Default::default()
            })
        } else {
            let tool_result = request
                .messages
                .iter()
                .rev()
                .find(|m| m.role == "tool")
                .and_then(|m| m.content.clone())
                .unwrap_or_else(|| "<no tool result>".into());
            Ok(ChatResponse {
                content: Some(format!("FINAL:{}", tool_result)),
                ..Default::default()
            })
        }
    }
}

fn fixture(project_dir: &Path) -> (mlua::Lua, Arc<InputBridge>) {
    let lua = create_crew_lua().expect("create_crew_lua");

    let provider = Box::new(AskingProvider {
        calls: AtomicUsize::new(0),
    });
    // Runtime::new registers the built-in registry, including AskHumanTool.
    let runtime = Arc::new(Runtime::new(provider, Some(project_dir)));
    runtime.set_self_ref(Arc::downgrade(&runtime));

    lua.set_app_data(runtime.clone());
    lua.set_app_data(Arc::new(project_dir.to_path_buf()));
    register_crew_constructor(&lua, runtime, Vec::new(), project_dir.to_path_buf())
        .expect("register_crew_constructor");

    // Same app-data wiring the serve handler / cmd_run perform; crew:run()
    // re-binds run_id + store + eventbus onto it.
    let bridge = Arc::new(InputBridge::new(BridgeMode::Http));
    lua.set_app_data(AskHumanContext {
        bridge: bridge.clone(),
        run_id: None,
        store: None,
        eventbus: None,
    });

    (lua, bridge)
}

const RUN_FLOW: &str = r#"
    -- No api_key / base_url: the crew must fall back to the RUNTIME
    -- provider (the scripted AskingProvider) instead of constructing a
    -- real per-crew OpenAI client.
    local crew = Crew.new({
        goal = "agent-initiated ask test",
        provider = "openai",
        model = "scripted",
    })
    crew:add_agent({
        name = "analyst",
        goal = "analyze the requested quarter",
        tools = { "ask_human" },
    })
    crew:add_task({
        name = "analyze",
        description = "Analyze the data",
        expected_output = "analysis",
        -- Deliberately SHORTER than the human answer delay below: proves
        -- that human-wait time does not count against the task timeout.
        timeout_secs = 2,
    })
    local results = crew:run()
    return results[1].output
"#;

#[tokio::test]
async fn agent_asks_human_mid_task_and_answer_reaches_the_model() {
    let dir = tempfile::tempdir().unwrap();
    let (lua, bridge) = fixture(dir.path());

    let flow = tokio::spawn(async move { lua.load(RUN_FLOW).eval_async::<String>().await });

    // The agent's question surfaces on the shared bridge, attributed to it.
    let question_id = loop {
        if let Some(q) = bridge.list().first() {
            assert_eq!(q.prompt, "[analyst] Which quarter should I analyze?");
            assert_eq!(q.choices, vec!["Q1".to_string(), "Q2".to_string()]);
            break q.question_id.clone();
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    };

    // Answer AFTER the 2s task timeout would have fired — the pause-aware
    // clock must keep the task alive while the question is pending.
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    bridge
        .answer(&question_id, serde_json::json!("Q2"))
        .unwrap();

    let output = flow.await.unwrap().expect("flow should succeed");
    assert_eq!(
        output, "FINAL:Q2",
        "the human's answer must reach the model's next turn verbatim"
    );
}

#[tokio::test]
async fn agent_ask_timeout_returns_soft_result_to_model() {
    let dir = tempfile::tempdir().unwrap();
    let (lua, _bridge) = fixture(dir.path());

    // Nobody answers. The full timeout→soft-result path is covered at the
    // tool level (timeout_returns_soft_result_not_error); here we assert
    // the task-runner half: an unanswered ask must NOT fail the task via
    // its own 2s `timeout_secs` — the pause-aware clock keeps it alive
    // while the question is pending.
    let flow = tokio::spawn(async move { lua.load(RUN_FLOW).eval_async::<String>().await });
    tokio::time::sleep(std::time::Duration::from_secs(4)).await;
    assert!(
        !flow.is_finished(),
        "task must still be alive while its question is pending (pause-aware timeout)"
    );
    flow.abort();
}
