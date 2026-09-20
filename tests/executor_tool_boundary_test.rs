use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use async_trait::async_trait;
use ironcrew::engine::agent::Agent;
use ironcrew::engine::executor::execute_task_standalone;
use ironcrew::engine::task::Task;
use ironcrew::llm::provider::{
    ChatRequest, ChatResponse, LlmProvider, ToolCallFunction, ToolCallRequest, ToolSchema,
};
use ironcrew::tools::registry::ToolRegistry;
use ironcrew::tools::{Tool, ToolCallContext};
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

struct MalformedToolProvider {
    rounds: AtomicUsize,
    saw_explicit_error: AtomicBool,
}

#[async_trait]
impl LlmProvider for MalformedToolProvider {
    async fn chat(&self, _request: ChatRequest) -> Result<ChatResponse> {
        panic!("the agent has a tool, so chat_with_tools must be used")
    }

    async fn chat_with_tools(
        &self,
        request: ChatRequest,
        _tools: &[ToolSchema],
    ) -> Result<ChatResponse> {
        match self.rounds.fetch_add(1, Ordering::SeqCst) {
            0 => Ok(ChatResponse {
                tool_calls: vec![ToolCallRequest {
                    id: "malformed-call".into(),
                    call_type: "function".into(),
                    function: ToolCallFunction {
                        name: "counting_tool".into(),
                        arguments: r#"{"value":"secret-canary""#.into(),
                    },
                }],
                ..Default::default()
            }),
            1 => {
                let saw_explicit_error = request.messages.iter().any(|message| {
                    message.role == "tool"
                        && message.tool_call_id.as_deref() == Some("malformed-call")
                        && message.content.as_deref() == Some("Tool error: invalid JSON arguments")
                });
                self.saw_explicit_error
                    .store(saw_explicit_error, Ordering::SeqCst);
                Ok(ChatResponse {
                    content: Some("recovered after malformed arguments".into()),
                    ..Default::default()
                })
            }
            _ => panic!("executor made an unexpected provider request"),
        }
    }
}

struct CountingTool {
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Tool for CountingTool {
    fn name(&self) -> &str {
        "counting_tool"
    }

    fn description(&self) -> &str {
        "Records whether it was executed"
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: self.name().into(),
            description: self.description().into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": { "value": { "type": "string" } },
                "required": ["value"]
            }),
        }
    }

    async fn execute(&self, _args: serde_json::Value, _ctx: &ToolCallContext) -> Result<String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok("executed".into())
    }
}

#[tokio::test]
async fn malformed_tool_arguments_return_an_explicit_error_without_execution() {
    let provider = MalformedToolProvider {
        rounds: AtomicUsize::new(0),
        saw_explicit_error: AtomicBool::new(false),
    };
    let tool_calls = Arc::new(AtomicUsize::new(0));
    let mut registry = ToolRegistry::new();
    registry.register(Box::new(CountingTool {
        calls: Arc::clone(&tool_calls),
    }));
    let task = Task {
        name: "malformed-tool-task".into(),
        description: "Use the counting tool".into(),
        ..Default::default()
    };
    let agent = Agent {
        name: "tool-agent".into(),
        goal: "Use tools safely".into(),
        tools: vec!["counting_tool".into()],
        ..Default::default()
    };

    let (output, _, _) = execute_task_standalone(
        &task,
        &agent,
        &provider,
        &registry,
        &HashMap::new(),
        "test-model",
        2,
        "",
        "",
        false,
    )
    .await
    .expect("the provider can recover after the explicit tool error");

    assert_eq!(output, "recovered after malformed arguments");
    assert_eq!(provider.rounds.load(Ordering::SeqCst), 2);
    assert!(provider.saw_explicit_error.load(Ordering::SeqCst));
    assert_eq!(tool_calls.load(Ordering::SeqCst), 0);
}
