use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use ironcrew::engine::agent::Agent;
use ironcrew::engine::executor::execute_task_standalone;
use ironcrew::engine::task::Task;
use ironcrew::llm::provider::{
    ChatRequest, ChatResponse, LlmProvider, ToolCallFunction, ToolCallRequest, ToolSchema,
};
use ironcrew::tools::registry::ToolRegistry;
use ironcrew::utils::error::{IronCrewError, Result};

struct UnsolicitedToolProvider {
    calls: AtomicUsize,
}

#[async_trait]
impl LlmProvider for UnsolicitedToolProvider {
    async fn chat(&self, _request: ChatRequest) -> Result<ChatResponse> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        if call == 0 {
            return Ok(ChatResponse {
                tool_calls: vec![ToolCallRequest {
                    id: "unexpected-call".into(),
                    call_type: "function".into(),
                    function: ToolCallFunction {
                        name: "unavailable_tool".into(),
                        arguments: r#"{"credential":"secret-canary"}"#.into(),
                    },
                }],
                ..Default::default()
            });
        }

        Ok(ChatResponse {
            content: Some("a follow-up request escaped the boundary".into()),
            ..Default::default()
        })
    }

    async fn chat_with_tools(
        &self,
        _request: ChatRequest,
        _tools: &[ToolSchema],
    ) -> Result<ChatResponse> {
        panic!("no tool schemas should be supplied for a tool-free agent")
    }
}

#[tokio::test]
async fn tool_free_task_rejects_unsolicited_tool_calls_before_follow_up() {
    let provider = UnsolicitedToolProvider {
        calls: AtomicUsize::new(0),
    };
    let task = Task {
        name: "bounded-task".into(),
        description: "Return a result without tools".into(),
        ..Default::default()
    };
    let agent = Agent {
        name: "tool-free-agent".into(),
        goal: "Answer directly".into(),
        ..Default::default()
    };
    let registry = ToolRegistry::new();

    let error = execute_task_standalone(
        &task,
        &agent,
        &provider,
        &registry,
        &HashMap::new(),
        "test-model",
        1,
        "",
        "",
        false,
    )
    .await
    .expect_err("an unsolicited tool call must fail closed");

    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    assert!(matches!(error, IronCrewError::Provider(_)));
    let rendered = error.to_string();
    assert!(rendered.contains("no tools were supplied"));
    assert!(!rendered.contains("unavailable_tool"));
    assert!(!rendered.contains("secret-canary"));
}
