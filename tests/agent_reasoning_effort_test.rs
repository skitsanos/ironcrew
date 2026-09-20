//! Regression: a per-agent `reasoning_effort` reaches the provider request.

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use ironcrew::engine::agent::Agent;
use ironcrew::engine::executor::execute_task_standalone;
use ironcrew::engine::task::Task;
use ironcrew::llm::provider::{ChatRequest, ChatResponse, LlmProvider, ToolSchema};
use ironcrew::tools::registry::ToolRegistry;
use ironcrew::utils::error::Result;

#[derive(Default)]
struct RecordingProvider {
    efforts: Mutex<Vec<Option<String>>>,
}

#[async_trait]
impl LlmProvider for RecordingProvider {
    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse> {
        self.efforts
            .lock()
            .unwrap()
            .push(request.reasoning_effort.clone());
        Ok(ChatResponse {
            content: Some("done".into()),
            ..Default::default()
        })
    }

    async fn chat_with_tools(
        &self,
        request: ChatRequest,
        _tools: &[ToolSchema],
    ) -> Result<ChatResponse> {
        self.chat(request).await
    }
}

#[tokio::test]
async fn agent_reasoning_effort_is_forwarded_on_the_request() {
    let provider = RecordingProvider::default();
    let task = Task {
        name: "deep-analysis".into(),
        description: "Reason carefully".into(),
        ..Default::default()
    };
    let agent = Agent {
        name: "analyst".into(),
        goal: "analyze".into(),
        reasoning_effort: Some("high".into()),
        ..Default::default()
    };
    let registry = ToolRegistry::new();
    let result = execute_task_standalone(
        &task,
        &agent,
        &provider,
        &registry,
        &HashMap::new(),
        "gpt-5.6-luna",
        3,
        "",
        "",
        false,
    )
    .await
    .expect("task completes");
    let (output, _reasoning, _usage) = result;
    assert_eq!(output, "done");
    assert_eq!(
        provider.efforts.lock().unwrap().as_slice(),
        &[Some("high".to_string())]
    );
}
