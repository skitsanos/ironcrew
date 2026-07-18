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

use std::ops::{Deref, DerefMut};
use std::sync::Arc;
use std::time::Duration;

use crate::engine::agent::Agent;
use crate::engine::eventbus::CrewEvent;
use crate::llm::provider::{
    ChatMessage, ChatRequest, DEFAULT_CHAT_HISTORY_MAX_MESSAGES, HARD_CHAT_HISTORY_MAX_MESSAGES,
    LlmProvider, append_text_bounded, chat_history_max_bytes, enforce_conversation_history_limits,
    max_reasoning_bytes, validate_chat_history,
};
use crate::tools::ToolCallContext;
use crate::utils::error::{IronCrewError, Result};

/// Rolls back the currently-active user turn if the future is cancelled or
/// returns before `commit`. Keeping this guard alive across provider/tool
/// awaits makes Tokio timeout and shutdown cancellation history-safe.
pub(crate) struct ActiveTurnGuard<'a> {
    history: &'a mut Vec<ChatMessage>,
    active_user_present: bool,
    committed: bool,
}

impl<'a> ActiveTurnGuard<'a> {
    pub(crate) fn new(history: &'a mut Vec<ChatMessage>) -> Self {
        let active_user_present = history.last().is_some_and(|message| message.role == "user");
        Self {
            history,
            active_user_present,
            committed: false,
        }
    }

    pub(crate) fn commit(&mut self) {
        self.committed = true;
    }
}

impl Deref for ActiveTurnGuard<'_> {
    type Target = Vec<ChatMessage>;

    fn deref(&self) -> &Self::Target {
        self.history
    }
}

impl DerefMut for ActiveTurnGuard<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.history
    }
}

impl Drop for ActiveTurnGuard<'_> {
    fn drop(&mut self) {
        if !self.committed && self.active_user_present {
            rollback_active_turn(self.history);
        }
    }
}

/// Returns the per-tool-call timeout in seconds.
///
/// Reads `IRONCREW_TOOL_TIMEOUT` from the environment. Missing, invalid, and
/// zero values fall back to 60 seconds; excessive values are clamped to one
/// hour so a malformed deployment setting cannot leave tool futures alive
/// indefinitely.
const DEFAULT_TOOL_TIMEOUT_SECS: u64 = 60;
const MAX_TOOL_TIMEOUT_SECS: u64 = 3_600;

pub(crate) fn tool_timeout_secs() -> u64 {
    match std::env::var("IRONCREW_TOOL_TIMEOUT")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
    {
        Some(value) if value > 0 => value.min(MAX_TOOL_TIMEOUT_SECS),
        _ => DEFAULT_TOOL_TIMEOUT_SECS,
    }
}

/// Run a single send/respond round (with tool-call loop) against `agent`
/// using `provider` and the caller-owned `history` buffer. Returns the
/// final assistant text and any accumulated reasoning.
///
/// * `max_tool_rounds` caps how many tool-call rounds the helper will
///   process before returning an error. Each round corresponds to one
///   assistant tool_calls response followed by one or more tool results.
/// * `max_history` trims the oldest complete user-turn groups after every
///   append. `None` selects the safe default; zero and values above the hard
///   ceiling are rejected.
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
    history_buffer: &mut Vec<ChatMessage>,
    ctx: &ToolCallContext,
) -> Result<(String, Option<String>)> {
    let mut history = ActiveTurnGuard::new(history_buffer);
    let max_history = match max_history {
        Some(value) if (1..=HARD_CHAT_HISTORY_MAX_MESSAGES).contains(&value) => value,
        Some(value) => {
            return Err(IronCrewError::Validation(format!(
                "max_history must be between 1 and {HARD_CHAT_HISTORY_MAX_MESSAGES}, got {value}"
            )));
        }
        None => DEFAULT_CHAT_HISTORY_MAX_MESSAGES,
    };
    let max_history_bytes = chat_history_max_bytes();
    enforce_conversation_history_limits(&mut history, max_history, max_history_bytes)?;
    validate_chat_history(&history, max_history, max_history_bytes, true)?;

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
    let reasoning_limit = max_reasoning_bytes();
    let mut reasoning_truncated = false;
    let mut rounds = 0usize;

    loop {
        validate_chat_history(&history, max_history, max_history_bytes, true)?;
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
                reasoning_truncated |=
                    append_text_bounded(&mut accumulated_reasoning, "\n", reasoning_limit);
            }
            reasoning_truncated |=
                append_text_bounded(&mut accumulated_reasoning, r, reasoning_limit);

            if let Some(bus) = &ctx.eventbus {
                let mut bounded = String::new();
                append_text_bounded(&mut bounded, r, reasoning_limit);
                bus.emit(CrewEvent::TaskThinking {
                    task: scope.clone(),
                    agent: agent.name.clone(),
                    content: bounded,
                });
            }
        }

        // No tool calls → final assistant reply, append + return.
        if response.tool_calls.is_empty() {
            let content = response
                .content
                .ok_or_else(|| IronCrewError::Provider("Empty response from LLM".into()))?;

            history.push(ChatMessage::assistant(Some(content.clone()), None));
            enforce_conversation_history_limits(&mut history, max_history, max_history_bytes)?;

            let reasoning = if accumulated_reasoning.is_empty() {
                None
            } else {
                if reasoning_truncated {
                    tracing::warn!(
                        agent = %agent.name,
                        limit = reasoning_limit,
                        "Reasoning text was truncated to the configured byte limit"
                    );
                }
                Some(accumulated_reasoning)
            };
            history.commit();
            return Ok((content, reasoning));
        }

        rounds += 1;
        if rounds > max_tool_rounds {
            return Err(IronCrewError::Validation(format!(
                "Agent '{}' exceeded max tool rounds ({})",
                agent.name, max_tool_rounds
            )));
        }

        // Append the assistant's tool-call request to history, carrying the
        // provider's reasoning blocks for replay (extended thinking + tools).
        history.push(ChatMessage::assistant_with_blocks(
            response.content.clone(),
            Some(response.tool_calls.clone()),
            response.raw_blocks.clone(),
        ));
        enforce_conversation_history_limits(&mut history, max_history, max_history_bytes)?;

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

            // A call may extend its own dispatch deadline (ask_human waits
            // on a person; approval-gated tools wait for a sign-off);
            // everything else gets the global timeout.
            let timeout = ctx
                .tool_registry
                .as_ref()
                .and_then(|reg| reg.dispatch_timeout(&tool_call.function.name, &args))
                .unwrap_or_else(|| Duration::from_secs(tool_timeout_secs()));
            let (result_text, ok) = match &ctx.tool_registry {
                Some(reg) => {
                    let dispatch = reg.execute(&tool_call.function.name, args, ctx);
                    match tokio::time::timeout(timeout, dispatch).await {
                        Ok(Ok(s)) => (s, true),
                        Ok(Err(e)) => (format!("Tool error: {}", e), false),
                        Err(_) => (
                            format!("Tool error: Tool timed out after {}s", timeout.as_secs()),
                            false,
                        ),
                    }
                }
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
            enforce_conversation_history_limits(&mut history, max_history, max_history_bytes)?;
        }
    }
}

fn rollback_active_turn(history: &mut Vec<ChatMessage>) {
    if let Some(active_start) = history.iter().rposition(|message| message.role == "user") {
        history.truncate(active_start);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Arc;

    use async_trait::async_trait;
    use serde_json::json;

    use crate::engine::agent::Agent;
    use crate::llm::provider::{
        ChatRequest, ChatResponse, LlmProvider, ToolCallFunction, ToolCallRequest, ToolSchema,
    };
    use crate::tools::registry::ToolRegistry;
    use crate::tools::{Tool, ToolCallContext};
    use crate::utils::error::Result;

    // ── Stub provider ────────────────────────────────────────────────────────

    /// On the first call returns a response asking for `sleepy_tool`; on every
    /// subsequent call returns a plain text reply.
    struct CannedProvider;

    #[async_trait]
    impl LlmProvider for CannedProvider {
        async fn chat(&self, _request: ChatRequest) -> Result<ChatResponse> {
            Ok(ChatResponse {
                content: Some("final answer".into()),
                reasoning: None,
                tool_calls: vec![],
                usage: None,
                raw_blocks: None,
            })
        }

        async fn chat_with_tools(
            &self,
            request: ChatRequest,
            _tools: &[ToolSchema],
        ) -> Result<ChatResponse> {
            // If the history already contains a tool-result message, we're on
            // the second round — return the final reply.
            let already_has_tool_result = request.messages.iter().any(|m| m.role == "tool");

            if already_has_tool_result {
                return Ok(ChatResponse {
                    content: Some("final answer".into()),
                    reasoning: None,
                    tool_calls: vec![],
                    usage: None,
                    raw_blocks: None,
                });
            }

            // First round: ask the model to call `sleepy_tool`.
            Ok(ChatResponse {
                content: None,
                reasoning: None,
                tool_calls: vec![ToolCallRequest {
                    id: "call-1".into(),
                    call_type: "function".into(),
                    function: ToolCallFunction {
                        name: "sleepy_tool".into(),
                        arguments: "{}".into(),
                    },
                }],
                usage: None,
                raw_blocks: None,
            })
        }
    }

    struct SlowProvider;

    #[async_trait]
    impl LlmProvider for SlowProvider {
        async fn chat(&self, _request: ChatRequest) -> Result<ChatResponse> {
            tokio::time::sleep(Duration::from_secs(10)).await;
            Ok(ChatResponse {
                content: Some("too late".into()),
                reasoning: None,
                tool_calls: vec![],
                usage: None,
                raw_blocks: None,
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

    // ── Stub tool ────────────────────────────────────────────────────────────

    /// A tool that sleeps for 10 seconds, far longer than the 1-second timeout
    /// we'll configure in the test.
    struct SleepyTool;

    #[async_trait]
    impl Tool for SleepyTool {
        fn name(&self) -> &str {
            "sleepy_tool"
        }
        fn description(&self) -> &str {
            "Sleeps for a very long time"
        }
        fn schema(&self) -> ToolSchema {
            ToolSchema {
                name: "sleepy_tool".into(),
                description: "Sleeps for a very long time".into(),
                parameters: json!({"type": "object", "properties": {}}),
            }
        }
        async fn execute(
            &self,
            _args: serde_json::Value,
            _ctx: &ToolCallContext,
        ) -> Result<String> {
            tokio::time::sleep(Duration::from_secs(10)).await;
            Ok("done (should never reach caller)".into())
        }
    }

    // ── env-var serialisation guard ──────────────────────────────────────────

    /// Serializes tests that mutate process-wide env vars. `cargo test` runs
    /// tests in parallel threads inside a single process; without this lock the
    /// `IRONCREW_TOOL_TIMEOUT` reads/writes race between the three tests below.
    fn env_guard() -> std::sync::MutexGuard<'static, ()> {
        use std::sync::{Mutex, OnceLock};
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    // ── Tests ────────────────────────────────────────────────────────────────

    /// The per-tool timeout must fire when a tool hangs and the result message
    /// must contain the "timed out" string so callers can detect it.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn run_single_agent_turn_tool_timeout_returns_error_string() {
        let _guard = env_guard();
        // Use a 1-second timeout so the test finishes quickly.
        unsafe {
            std::env::set_var("IRONCREW_TOOL_TIMEOUT", "1");
        }

        let provider: Arc<dyn LlmProvider> = Arc::new(CannedProvider);

        let mut registry = ToolRegistry::new();
        registry.register(Box::new(SleepyTool));

        let agent = Agent {
            name: "timeout-tester".into(),
            goal: "test per-tool timeout".into(),
            tools: vec!["sleepy_tool".into()],
            ..Default::default()
        };

        let mut history = vec![
            ChatMessage::system("you are a tester"),
            ChatMessage::user("call the sleepy tool"),
        ];

        let ctx = ToolCallContext {
            tool_registry: Some(registry),
            ..Default::default()
        };

        // The turn must complete (not hang) and return the final LLM reply.
        let (content, _reasoning) = run_single_agent_turn(
            &agent,
            &provider,
            "stub-model",
            5,
            Some(50),
            &mut history,
            &ctx,
        )
        .await
        .expect("helper should not error; timeout surfaces as a tool-result, not an Err");

        assert_eq!(content, "final answer");

        // Find the tool-result message and confirm it contains "timed out".
        let tool_result_msg = history
            .iter()
            .find(|m| m.role == "tool")
            .expect("a tool-result message must exist in history");
        let result_text = tool_result_msg
            .content
            .as_deref()
            .expect("tool-result message must have content");
        assert!(
            result_text.contains("timed out"),
            "expected 'timed out' in tool result, got: {result_text:?}"
        );
        assert!(
            result_text.contains("1s"),
            "expected timeout duration in message, got: {result_text:?}"
        );

        // Clean up env var so subsequent tests in the same process aren't affected.
        unsafe {
            std::env::remove_var("IRONCREW_TOOL_TIMEOUT");
        }
    }

    #[tokio::test]
    async fn cancelled_turn_rolls_back_active_history() {
        let provider: Arc<dyn LlmProvider> = Arc::new(SlowProvider);
        let agent = Agent {
            name: "cancellation-tester".into(),
            goal: "test cancellation rollback".into(),
            ..Default::default()
        };
        let mut history = vec![
            ChatMessage::system("you are a tester"),
            ChatMessage::user("this turn must be rolled back"),
        ];

        let result = tokio::time::timeout(
            Duration::from_millis(10),
            run_single_agent_turn(
                &agent,
                &provider,
                "stub-model",
                5,
                Some(50),
                &mut history,
                &ToolCallContext::default(),
            ),
        )
        .await;

        assert!(result.is_err(), "the deliberately slow turn must time out");
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].role, "system");
    }

    /// Sanity-check `tool_timeout_secs()`: missing / unparseable env var falls
    /// back to 60.
    #[test]
    fn tool_timeout_secs_defaults_to_60() {
        let _guard = env_guard();
        unsafe {
            std::env::remove_var("IRONCREW_TOOL_TIMEOUT");
        }
        assert_eq!(tool_timeout_secs(), 60);
    }

    /// Sanity-check `tool_timeout_secs()`: valid env var is parsed.
    #[test]
    fn tool_timeout_secs_reads_env_var() {
        let _guard = env_guard();
        unsafe {
            std::env::set_var("IRONCREW_TOOL_TIMEOUT", "120");
        }
        assert_eq!(tool_timeout_secs(), 120);
        unsafe {
            std::env::remove_var("IRONCREW_TOOL_TIMEOUT");
        }
    }

    #[test]
    fn tool_timeout_secs_rejects_zero_and_invalid_values() {
        let _guard = env_guard();
        unsafe {
            std::env::set_var("IRONCREW_TOOL_TIMEOUT", "0");
        }
        assert_eq!(tool_timeout_secs(), DEFAULT_TOOL_TIMEOUT_SECS);

        unsafe {
            std::env::set_var("IRONCREW_TOOL_TIMEOUT", "not-a-number");
        }
        assert_eq!(tool_timeout_secs(), DEFAULT_TOOL_TIMEOUT_SECS);
        unsafe {
            std::env::remove_var("IRONCREW_TOOL_TIMEOUT");
        }
    }

    #[test]
    fn tool_timeout_secs_clamps_excessive_values() {
        let _guard = env_guard();
        unsafe {
            std::env::set_var("IRONCREW_TOOL_TIMEOUT", u64::MAX.to_string());
        }
        assert_eq!(tool_timeout_secs(), MAX_TOOL_TIMEOUT_SECS);
        unsafe {
            std::env::remove_var("IRONCREW_TOOL_TIMEOUT");
        }
    }
}
