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
use crate::llm::provider::{ChatMessage, ChatRequest, LlmProvider};
use crate::tools::ToolCallContext;
use crate::utils::error::{IronCrewError, Result};

/// Run a single send/respond round (with tool-call loop) against `agent`
/// using `provider` and the caller-owned `history` buffer. Returns the
/// final assistant text and any accumulated reasoning.
///
/// * `max_tool_rounds` caps how many tool-call rounds the helper will
///   process before returning an error. Each round corresponds to one
///   assistant tool_calls response followed by one or more tool results.
/// * `max_history` optionally trims the oldest non-system messages after
///   every append. `None` = unbounded.
/// * `ctx.tool_registry` supplies the tool schemas and dispatches each
///   tool call. `None` → no tools are advertised and tool dispatches
///   error with a "no tool registry available" message.
/// * `ctx.eventbus` is used to emit `ToolCall` / `ToolResult` /
///   `TaskThinking` events. `None` suppresses telemetry silently.
/// * `ctx.caller_scope` supplies the `task` field on events. When
///   absent, falls back to `agent__<agent.name>`.
#[allow(clippy::too_many_arguments)]
pub async fn run_single_agent_turn(
    agent: &Agent,
    provider: &Arc<dyn LlmProvider>,
    model: &str,
    max_tool_rounds: usize,
    max_history: Option<usize>,
    history: &mut Vec<ChatMessage>,
    ctx: &ToolCallContext,
) -> Result<(String, Option<String>)> {
    let tool_schemas = match &ctx.tool_registry {
        Some(reg) => reg.schemas_for(&agent.tools),
        None => Vec::new(),
    };
    let has_tools = !tool_schemas.is_empty();

    let scope = ctx
        .caller_scope
        .clone()
        .unwrap_or_else(|| format!("agent__{}", agent.name));

    let mut accumulated_reasoning = String::new();
    let mut rounds = 0usize;

    loop {
        let messages_snapshot: Vec<ChatMessage> = history.clone();
        let request = ChatRequest {
            messages: messages_snapshot,
            model: model.to_string(),
            temperature: agent.temperature,
            max_tokens: agent.max_tokens,
            response_format: agent.response_format.clone(),
            prompt_cache_key: None,
            prompt_cache_retention: None,
        };

        let response = if has_tools {
            provider.chat_with_tools(request, &tool_schemas).await?
        } else {
            provider.chat(request).await?
        };

        if let Some(ref r) = response.reasoning {
            if !accumulated_reasoning.is_empty() {
                accumulated_reasoning.push('\n');
            }
            accumulated_reasoning.push_str(r);

            if let Some(bus) = &ctx.eventbus {
                bus.emit(CrewEvent::TaskThinking {
                    task: scope.clone(),
                    agent: agent.name.clone(),
                    content: r.clone(),
                });
            }
        }

        // No tool calls → final assistant reply, append + return.
        if response.tool_calls.is_empty() {
            let content = response
                .content
                .ok_or_else(|| IronCrewError::Provider("Empty response from LLM".into()))?;

            history.push(ChatMessage::assistant(Some(content.clone()), None));
            enforce_history_cap(history, max_history);

            let reasoning = if accumulated_reasoning.is_empty() {
                None
            } else {
                Some(accumulated_reasoning)
            };
            return Ok((content, reasoning));
        }

        rounds += 1;
        if rounds > max_tool_rounds {
            return Err(IronCrewError::Validation(format!(
                "Agent '{}' exceeded max tool rounds ({})",
                agent.name, max_tool_rounds
            )));
        }

        // Append the assistant's tool-call request to history.
        history.push(ChatMessage::assistant(
            response.content.clone(),
            Some(response.tool_calls.clone()),
        ));
        enforce_history_cap(history, max_history);

        // Execute each tool call and append its result.
        for tool_call in &response.tool_calls {
            let started = std::time::Instant::now();
            if let Some(bus) = &ctx.eventbus {
                bus.emit(CrewEvent::ToolCall {
                    task: scope.clone(),
                    tool: tool_call.function.name.clone(),
                });
            }

            let args: serde_json::Value = serde_json::from_str(&tool_call.function.arguments)
                .unwrap_or(serde_json::Value::Null);

            let (result_text, ok) = match &ctx.tool_registry {
                Some(reg) => match reg.execute(&tool_call.function.name, args, ctx).await {
                    Ok(s) => (s, true),
                    Err(e) => (format!("Tool error: {}", e), false),
                },
                None => (
                    format!(
                        "Tool error: no tool registry available to dispatch {}",
                        tool_call.function.name
                    ),
                    false,
                ),
            };

            if let Some(bus) = &ctx.eventbus {
                bus.emit(CrewEvent::ToolResult {
                    task: scope.clone(),
                    tool: tool_call.function.name.clone(),
                    success: ok,
                    duration_ms: started.elapsed().as_millis() as u64,
                });
            }

            history.push(ChatMessage::tool(&tool_call.id, &result_text));
            enforce_history_cap(history, max_history);
        }
    }
}

/// Trim `history` in-place if it exceeds `limit` non-system messages.
/// Preserves the system message at index 0.
fn enforce_history_cap(history: &mut Vec<ChatMessage>, max_history: Option<usize>) {
    let Some(cap) = max_history else {
        return;
    };
    // +1 for the system message we always keep
    let limit = cap + 1;
    if history.len() <= limit {
        return;
    }
    let excess = history.len() - limit;
    history.drain(1..1 + excess);
}
