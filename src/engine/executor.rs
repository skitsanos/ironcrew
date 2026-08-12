use std::collections::HashMap;

mod hooks;

use hooks::{run_after_hook, run_before_hook};

use crate::engine::agent::Agent;
use crate::engine::interpolate::{interpolate_bounded, prompt_char_limit};
use crate::engine::task::{Task, TaskResult, TaskTokenUsage};
use crate::llm::provider::*;
use crate::tools::ToolCallContext;
use crate::tools::registry::ToolRegistry;
use crate::utils::error::{IronCrewError, Result};

const PROMPT_TRUNCATION_MARKER: &str = "\n\n[... prompt truncated due to size limit]";

/// Incrementally builds a character-bounded prompt. This avoids first joining
/// every dependency/tool output into one unbounded temporary allocation and
/// only then truncating it.
struct BoundedPrompt {
    text: String,
    max_chars: usize,
    chars: usize,
    truncated: bool,
}

impl BoundedPrompt {
    fn new(max_chars: usize) -> Self {
        Self {
            text: String::with_capacity(max_chars.min(16 * 1024)),
            max_chars,
            chars: 0,
            truncated: false,
        }
    }

    fn push(&mut self, value: &str) {
        if self.truncated || self.chars >= self.max_chars {
            self.truncated |= !value.is_empty();
            return;
        }

        let remaining = self.max_chars - self.chars;
        let mut count = 0usize;
        let mut boundary = value.len();
        for (byte_index, _) in value.char_indices() {
            if count == remaining {
                boundary = byte_index;
                self.truncated = true;
                break;
            }
            count += 1;
        }
        self.text.push_str(&value[..boundary]);
        self.chars += count.min(remaining);
    }

    fn section(&mut self, label: &str, value: &str) {
        if !self.text.is_empty() {
            self.push("\n\n");
        }
        self.push(label);
        self.push(value);
    }

    fn finish(mut self) -> (String, bool) {
        if self.truncated {
            let marker_chars = PROMPT_TRUNCATION_MARKER.chars().count();
            let keep_chars = self.max_chars.saturating_sub(marker_chars);
            if let Some((boundary, _)) = self.text.char_indices().nth(keep_chars) {
                self.text.truncate(boundary);
            }
            let remaining = self.max_chars.saturating_sub(self.text.chars().count());
            let marker_boundary = PROMPT_TRUNCATION_MARKER
                .char_indices()
                .nth(remaining)
                .map(|(index, _)| index)
                .unwrap_or(PROMPT_TRUNCATION_MARKER.len());
            self.text
                .push_str(&PROMPT_TRUNCATION_MARKER[..marker_boundary]);
        }
        self.text.shrink_to_fit();
        (self.text, self.truncated)
    }
}

pub struct TaskExecutionContext<'a> {
    pub task: &'a Task,
    pub agent: &'a Agent,
    pub provider: &'a dyn LlmProvider,
    pub tool_registry: &'a ToolRegistry,
    pub completed_results: &'a HashMap<String, TaskResult>,
    pub model: String,
    pub max_tool_rounds: usize,
    pub memory_context: String,
    pub messages_context: String,
    pub should_stream: bool,
    pub prompt_cache_key: Option<String>,
    pub prompt_cache_retention: Option<String>,
    pub before_task_hook: Option<&'a [u8]>,
    pub after_task_hook: Option<&'a [u8]>,
    /// Human-input transport for the agent-facing `ask_human` tool.
    pub ask_human: Option<&'a crate::engine::input_bridge::AskHumanContext>,
}

impl<'a> TaskExecutionContext<'a> {
    pub async fn execute(&self) -> Result<(String, Option<String>, Option<TaskTokenUsage>)> {
        let max_prompt_chars = prompt_char_limit();
        // Run before_task hook if present
        let raw_description = interpolate_bounded(
            &self.task.description,
            self.completed_results,
            max_prompt_chars,
        )
        .0;
        let description = if let Some(bytecode) = self.before_task_hook {
            run_before_hook(bytecode, &self.task.name, &raw_description)
        } else {
            raw_description
        };

        let mut messages = Vec::new();
        let mut total_usage = TaskTokenUsage::default();
        let mut accumulated_reasoning = String::new();
        let reasoning_limit = max_reasoning_bytes();
        let mut reasoning_truncated = false;
        let history_max_bytes = chat_history_max_bytes();

        // System prompt
        let system_content = self.agent.system_prompt.clone().unwrap_or_else(|| {
            format!(
                "You are {}. Your goal: {}",
                self.agent.name, self.agent.goal
            )
        });
        messages.push(ChatMessage::system(&system_content));
        let expected_output = self
            .task
            .expected_output
            .as_ref()
            .map(|s| interpolate_bounded(s, self.completed_results, max_prompt_chars).0);
        let context = self
            .task
            .context
            .as_ref()
            .map(|s| interpolate_bounded(s, self.completed_results, max_prompt_chars).0);

        // Cap total prompt size while constructing it, so a large collection
        // of dependency outputs never creates an unbounded temporary string.
        let mut prompt = BoundedPrompt::new(max_prompt_chars);
        prompt.section("Task: ", &description);
        if let Some(ref expected) = expected_output {
            prompt.section("Expected output: ", expected);
        }
        if let Some(ref ctx) = context {
            prompt.section("Additional context: ", ctx);
        }
        if !self.memory_context.is_empty() {
            prompt.section("Relevant memory:\n", &self.memory_context);
        }
        if !self.messages_context.is_empty() {
            prompt.section("", &self.messages_context);
        }
        for dep_name in &self.task.depends_on {
            if let Some(dep_result) = self.completed_results.get(dep_name)
                && dep_result.success
            {
                prompt.section(&format!("Result from '{}': ", dep_name), &dep_result.output);
            }
        }
        let (user_prompt, truncated) = prompt.finish();
        if truncated {
            tracing::warn!(
                "Task '{}': prompt truncated to {} chars",
                self.task.name,
                max_prompt_chars
            );
        }

        messages.push(ChatMessage::user(&user_prompt));

        // Get tool schemas for this agent
        let tool_schemas = self.tool_registry.schemas_for(&self.agent.tools);
        let has_tools = !tool_schemas.is_empty();

        let mut rounds = 0;

        loop {
            validate_chat_history(
                &messages,
                HARD_CHAT_HISTORY_MAX_MESSAGES,
                history_max_bytes,
                true,
            )?;
            let request = ChatRequest {
                messages: messages.clone(),
                model: self.model.to_string(),
                temperature: self.agent.temperature,
                max_tokens: self.agent.max_tokens,
                response_format: self.agent.response_format.clone(),
                prompt_cache_key: self.prompt_cache_key.clone(),
                prompt_cache_retention: self.prompt_cache_retention.clone(),
            };

            let response = if self.should_stream && !has_tools {
                // Stream mode: print chunks to stderr as they arrive
                let (tx, mut rx) = tokio::sync::mpsc::channel::<StreamChunk>(100);

                let print_handle = tokio::spawn(async move {
                    use std::io::Write;
                    while let Some(chunk) = rx.recv().await {
                        match chunk {
                            StreamChunk::Text(text) => {
                                eprint!("{}", text);
                                std::io::stderr().flush().ok();
                            }
                            StreamChunk::Thinking(text) => {
                                // Dim color for reasoning — visually distinct from output
                                eprint!("\x1b[90m{}\x1b[0m", text);
                                std::io::stderr().flush().ok();
                            }
                            StreamChunk::Done => {
                                eprintln!(); // newline at end
                            }
                            StreamChunk::Error(e) => {
                                eprintln!("\n[Stream error: {}]", e);
                            }
                            _ => {}
                        }
                    }
                });

                let result = self.provider.chat_stream(request, tx).await;
                print_handle.await.ok();
                result?
            } else if has_tools {
                self.provider
                    .chat_with_tools(request, &tool_schemas)
                    .await?
            } else {
                self.provider.chat(request).await?
            };

            // Accumulate token usage
            if let Some(usage) = &response.usage {
                total_usage.prompt_tokens = total_usage
                    .prompt_tokens
                    .saturating_add(usage.prompt_tokens);
                total_usage.completion_tokens = total_usage
                    .completion_tokens
                    .saturating_add(usage.completion_tokens);
                total_usage.total_tokens =
                    total_usage.total_tokens.saturating_add(usage.total_tokens);
                total_usage.cached_tokens = total_usage
                    .cached_tokens
                    .saturating_add(usage.cached_tokens);
            }

            // Accumulate reasoning content across tool-call rounds
            if let Some(ref reasoning) = response.reasoning {
                if !accumulated_reasoning.is_empty() {
                    reasoning_truncated |=
                        append_text_bounded(&mut accumulated_reasoning, "\n", reasoning_limit);
                }
                reasoning_truncated |=
                    append_text_bounded(&mut accumulated_reasoning, reasoning, reasoning_limit);
            }

            // If no tool calls, return the content
            if response.tool_calls.is_empty() {
                let has_usage = total_usage.total_tokens > 0;
                let content = response
                    .content
                    .ok_or_else(|| IronCrewError::Provider("Empty response from LLM".into()))?;

                // Run after_task hook if present
                let final_output = if let Some(bytecode) = self.after_task_hook {
                    run_after_hook(bytecode, &self.task.name, &content, true)
                } else {
                    content
                };

                let reasoning = if accumulated_reasoning.is_empty() {
                    None
                } else {
                    if reasoning_truncated {
                        tracing::warn!(
                            task = %self.task.name,
                            limit = reasoning_limit,
                            "Reasoning text was truncated to the configured byte limit"
                        );
                    }
                    Some(accumulated_reasoning)
                };

                return Ok((
                    final_output,
                    reasoning,
                    if has_usage { Some(total_usage) } else { None },
                ));
            }

            if !has_tools {
                return Err(IronCrewError::Provider(
                    "Provider returned tool calls when no tools were supplied".into(),
                ));
            }

            rounds += 1;
            if rounds > self.max_tool_rounds {
                return Err(IronCrewError::Task {
                    task: self.task.name.clone(),
                    message: format!("Exceeded max tool rounds ({})", self.max_tool_rounds),
                });
            }

            // Add assistant message with tool calls (must include the tool_calls
            // array) and the provider's reasoning blocks, so extended-thinking
            // providers can replay them on the next round.
            messages.push(ChatMessage::assistant_with_blocks(
                response.content.clone(),
                Some(response.tool_calls.clone()),
                response.raw_blocks.clone(),
            ));
            validate_chat_history(
                &messages,
                HARD_CHAT_HISTORY_MAX_MESSAGES,
                history_max_bytes,
                false,
            )?;

            // Execute tool calls and add tool result messages
            for tool_call in &response.tool_calls {
                tracing::info!(
                    "Executing tool '{}' for task '{}'",
                    tool_call.function.name,
                    self.task.name
                );

                let args: serde_json::Value = serde_json::from_str(&tool_call.function.arguments)
                    .unwrap_or(serde_json::Value::Object(serde_json::Map::new()));

                let tool_timeout = self
                    .tool_registry
                    .dispatch_timeout(&tool_call.function.name, &args)
                    .unwrap_or_else(|| {
                        std::time::Duration::from_secs(crate::lua::agent_turn::tool_timeout_secs())
                    });

                let tool_ctx = ToolCallContext {
                    tool_registry: Some(self.tool_registry.clone()),
                    caller_agent: Some(self.agent.name.clone()),
                    caller_scope: Some(self.task.name.clone()),
                    ask_human: self.ask_human.cloned(),
                    ..ToolCallContext::default()
                };
                let tool_result = match tokio::time::timeout(
                    tool_timeout,
                    self.tool_registry
                        .execute(&tool_call.function.name, args, &tool_ctx),
                )
                .await
                {
                    Ok(result) => result,
                    Err(_) => Err(IronCrewError::ToolExecution {
                        tool: tool_call.function.name.clone(),
                        message: format!("Tool timed out after {}s", tool_timeout.as_secs()),
                    }),
                };

                let result_text = match tool_result {
                    Ok(output) => output,
                    Err(e) => format!("Tool error: {}", e),
                };

                messages.push(ChatMessage::tool(&tool_call.id, &result_text));
                validate_chat_history(
                    &messages,
                    HARD_CHAT_HISTORY_MAX_MESSAGES,
                    history_max_bytes,
                    false,
                )?;
            }
        }
    }
}

/// Backward-compatible wrapper that creates a TaskExecutionContext and executes it.
#[allow(clippy::too_many_arguments)]
pub async fn execute_task_standalone(
    task: &Task,
    agent: &Agent,
    provider: &dyn LlmProvider,
    tool_registry: &ToolRegistry,
    completed_results: &HashMap<String, TaskResult>,
    model: &str,
    max_tool_rounds: usize,
    memory_context: &str,
    messages_context: &str,
    should_stream: bool,
) -> Result<(String, Option<String>, Option<TaskTokenUsage>)> {
    execute_task_standalone_with_hooks(
        task,
        agent,
        provider,
        tool_registry,
        completed_results,
        model,
        max_tool_rounds,
        memory_context,
        messages_context,
        should_stream,
        None,
        None,
        None,
        None,
        None,
    )
    .await
}

/// Execute a task with optional prompt cache configuration and agent hooks.
#[allow(clippy::too_many_arguments)]
pub async fn execute_task_standalone_with_hooks(
    task: &Task,
    agent: &Agent,
    provider: &dyn LlmProvider,
    tool_registry: &ToolRegistry,
    completed_results: &HashMap<String, TaskResult>,
    model: &str,
    max_tool_rounds: usize,
    memory_context: &str,
    messages_context: &str,
    should_stream: bool,
    prompt_cache_key: Option<String>,
    prompt_cache_retention: Option<String>,
    before_task_hook: Option<&[u8]>,
    after_task_hook: Option<&[u8]>,
    ask_human: Option<&crate::engine::input_bridge::AskHumanContext>,
) -> Result<(String, Option<String>, Option<TaskTokenUsage>)> {
    let ctx = TaskExecutionContext {
        task,
        agent,
        provider,
        tool_registry,
        completed_results,
        model: model.to_string(),
        max_tool_rounds,
        memory_context: memory_context.to_string(),
        messages_context: messages_context.to_string(),
        should_stream,
        prompt_cache_key,
        prompt_cache_retention,
        before_task_hook,
        after_task_hook,
        ask_human,
    };
    ctx.execute().await
}

#[cfg(test)]
mod bounded_prompt_tests {
    use super::*;

    #[test]
    fn unicode_prompt_truncation_is_character_safe_and_bounded() {
        let mut prompt = BoundedPrompt::new(12);
        prompt.section("Task: ", "🦀🦀🦀🦀🦀🦀🦀🦀");
        let (text, truncated) = prompt.finish();
        assert!(truncated);
        assert!(text.chars().count() <= 12);
        assert!(std::str::from_utf8(text.as_bytes()).is_ok());
    }

    #[test]
    fn prompt_builder_stops_copying_after_limit() {
        let mut prompt = BoundedPrompt::new(64);
        prompt.section("Task: ", &"x".repeat(1_000_000));
        prompt.section("Result: ", &"y".repeat(1_000_000));
        let (text, truncated) = prompt.finish();
        assert!(truncated);
        assert!(text.len() <= 64);
    }
}
