//! Unit-level tests for `run_single_agent_turn` — the headless
//! single-turn agent helper extracted from `LuaConversationInner::run_turn`.
//!
//! No Lua, no runtime wiring — just a stub provider and a caller-owned
//! `Vec<ChatMessage>`. Tests the two primary code paths:
//!
//! 1. No tools configured → single provider call → assistant reply appended.
//! 2. (Future tasks add tool-call path coverage — the `run_turn` integration
//!    tests already exercise that branch through the real `LuaConversation`.)

use std::sync::Arc;

use async_trait::async_trait;
use ironcrew::engine::agent::Agent;
use ironcrew::llm::provider::{ChatMessage, ChatRequest, ChatResponse, LlmProvider, ToolSchema};
use ironcrew::lua::agent_turn::run_single_agent_turn;
use ironcrew::tools::ToolCallContext;
use ironcrew::utils::error::Result;

/// Provider that echoes the most recent user message back as
/// `stub-reply-<content>`. Ignores tool schemas (never asks the model to
/// call a tool) so the helper's no-tool-loop branch is exercised.
struct EchoProvider;

#[async_trait]
impl LlmProvider for EchoProvider {
    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse> {
        let last = request
            .messages
            .last()
            .and_then(|m| m.content.clone())
            .unwrap_or_default();
        Ok(ChatResponse {
            content: Some(format!("stub-reply-{}", last)),
            reasoning: None,
            tool_calls: vec![],
            usage: None,
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
async fn run_single_agent_turn_single_pass() {
    let provider: Arc<dyn LlmProvider> = Arc::new(EchoProvider);

    let agent = Agent {
        name: "tester".into(),
        goal: "test run_single_agent_turn".into(),
        ..Default::default()
    };

    let mut history = vec![
        ChatMessage::system("you are a tester"),
        ChatMessage::user("hello"),
    ];

    let ctx = ToolCallContext::default();
    let (content, reasoning) = run_single_agent_turn(
        &agent,
        &provider,
        "stub-model",
        5,
        Some(50),
        &mut history,
        &ctx,
    )
    .await
    .expect("helper returned Ok");

    assert_eq!(content, "stub-reply-hello");
    assert!(reasoning.is_none(), "no reasoning expected from stub");

    // History should now include the assistant reply.
    assert_eq!(history.len(), 3, "expected system + user + assistant");
    let last = history.last().expect("at least one message");
    assert_eq!(last.role, "assistant");
    assert_eq!(last.content.as_deref(), Some("stub-reply-hello"));
}
