use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use ironcrew::engine::agent::Agent;
use ironcrew::llm::provider::{
    ChatRequest, ChatResponse, LlmProvider, ToolCallFunction, ToolCallRequest, ToolSchema,
};
use ironcrew::tools::registry::ToolRegistry;
use ironcrew::tools::{Tool, ToolCallContext};
use ironcrew::utils::error::Result;

pub struct ScriptedProvider {
    replies: Mutex<VecDeque<ChatResponse>>,
    pub calls: AtomicUsize,
}

impl ScriptedProvider {
    pub fn new(replies: Vec<ChatResponse>) -> Arc<Self> {
        Arc::new(Self {
            replies: Mutex::new(replies.into()),
            calls: AtomicUsize::new(0),
        })
    }
}

#[async_trait]
impl LlmProvider for ScriptedProvider {
    async fn chat(&self, _request: ChatRequest) -> Result<ChatResponse> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(self
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected provider retry/call"))
    }

    async fn chat_with_tools(
        &self,
        request: ChatRequest,
        _tools: &[ToolSchema],
    ) -> Result<ChatResponse> {
        self.chat(request).await
    }
}

pub fn reply(content: Option<&str>) -> ChatResponse {
    ChatResponse {
        content: content.map(str::to_owned),
        reasoning: Some("A summary is not a final answer".into()),
        ..Default::default()
    }
}

pub fn tool_reply() -> ChatResponse {
    ChatResponse {
        tool_calls: vec![ToolCallRequest {
            id: "count-1".into(),
            call_type: "function".into(),
            function: ToolCallFunction {
                name: "count".into(),
                arguments: "{}".into(),
            },
        }],
        ..Default::default()
    }
}

pub fn agent(with_tool: bool) -> Agent {
    Agent {
        name: "worker".into(),
        goal: "Produce a final answer".into(),
        tools: if with_tool {
            vec!["count".into()]
        } else {
            vec![]
        },
        ..Default::default()
    }
}

struct CountingTool(Arc<AtomicUsize>);

#[async_trait]
impl Tool for CountingTool {
    fn name(&self) -> &str {
        "count"
    }
    fn description(&self) -> &str {
        "Count an external effect"
    }
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: self.name().into(),
            description: self.description().into(),
            parameters: serde_json::json!({"type": "object", "properties": {}}),
        }
    }
    async fn execute(&self, _args: serde_json::Value, _ctx: &ToolCallContext) -> Result<String> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok("effect completed".into())
    }
}

pub fn tool_registry() -> (ToolRegistry, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut registry = ToolRegistry::new();
    registry.register(Box::new(CountingTool(calls.clone())));
    (registry, calls)
}
