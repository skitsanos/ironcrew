//! Headless single-turn agent invocation.
//!
//! `run_single_agent_turn` drives one provider call + tool-call loop
//! against an agent, writing the full turn (user message, assistant
//! tool_calls, tool results, final assistant reply) into a caller-owned
//! `Vec<ChatMessage>`. Emits `ToolCall` / `ToolResult` / `TaskThinking`
//! events per tool dispatch and reasoning step. No `conversation_*`
//! events — those belong to session-tracked callers.
//!
//! Two callers:
//!   * `LuaConversationInner::run_turn` — threads `self.messages` in
//!   * `AgentAsTool::execute`          — fresh buffer per invocation
//!
//! Extracted from the original `LuaConversationInner::run_turn` body
//! so both paths share tool-loop logic without duplication.

use std::sync::Arc;

use crate::engine::agent::Agent;
use crate::engine::eventbus::CrewEvent;
use crate::llm::provider::{ChatMessage, LlmProvider};
use crate::tools::ToolCallContext;
use crate::utils::error::{IronCrewError, Result};

/// See module docs.
#[allow(dead_code, clippy::too_many_arguments)]
pub async fn run_single_agent_turn(
    agent: &Agent,
    provider: &Arc<dyn LlmProvider>,
    model: &str,
    max_tool_rounds: usize,
    max_history: Option<usize>,
    history: &mut Vec<ChatMessage>,
    ctx: &ToolCallContext,
) -> Result<(String, Option<String>)> {
    // Implemented in Task 6. This stub is registered so the module
    // compiles and downstream tasks can start referencing the symbol.
    let _ = (
        agent,
        provider,
        model,
        max_tool_rounds,
        max_history,
        history,
        ctx,
    );
    let _ = CrewEvent::Log {
        level: "error".into(),
        message: "run_single_agent_turn not yet implemented".into(),
    };
    Err(IronCrewError::Validation(
        "run_single_agent_turn not yet implemented (Task 5 stub)".into(),
    ))
}
