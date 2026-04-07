use std::collections::HashMap;

use crate::engine::agent::Agent;
use crate::engine::interpolate::interpolate;
use crate::engine::task::{Task, TaskResult, TaskTokenUsage};
use crate::llm::provider::*;
use crate::tools::registry::ToolRegistry;
use crate::utils::error::{IronCrewError, Result};

/// Run a before_task hook in a fresh Lua VM.
/// Returns the (possibly modified) task description.
fn run_before_hook(bytecode: &[u8], task_name: &str, task_description: &str) -> String {
    let lua = mlua::Lua::new();
    let func = match lua.load(bytecode).into_function() {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!(
                "before_task hook for task '{}' failed to load: {}",
                task_name,
                e
            );
            return task_description.to_string();
        }
    };

    match func.call::<mlua::Value>((task_name, task_description)) {
        Ok(mlua::Value::String(s)) => match s.to_str() {
            Ok(s) => s.to_string(),
            Err(_) => task_description.to_string(),
        },
        Ok(mlua::Value::Nil) => task_description.to_string(),
        Ok(_) => task_description.to_string(),
        Err(e) => {
            tracing::warn!("before_task hook for task '{}' failed: {}", task_name, e);
            task_description.to_string()
        }
    }
}

/// Run an after_task hook in a fresh Lua VM.
/// Returns the (possibly modified) output.
fn run_after_hook(bytecode: &[u8], task_name: &str, output: &str, success: bool) -> String {
    let lua = mlua::Lua::new();
    let func = match lua.load(bytecode).into_function() {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!(
                "after_task hook for task '{}' failed to load: {}",
                task_name,
                e
            );
            return output.to_string();
        }
    };

    match func.call::<mlua::Value>((task_name, output, success)) {
        Ok(mlua::Value::String(s)) => match s.to_str() {
            Ok(s) => s.to_string(),
            Err(_) => output.to_string(),
        },
        Ok(mlua::Value::Nil) => output.to_string(),
        Ok(_) => output.to_string(),
        Err(e) => {
            tracing::warn!("after_task hook for task '{}' failed: {}", task_name, e);
            output.to_string()
        }
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
}

impl<'a> TaskExecutionContext<'a> {
    pub async fn execute(&self) -> Result<(String, Option<TaskTokenUsage>)> {
        // Run before_task hook if present
        let raw_description = interpolate(&self.task.description, self.completed_results);
        let description = if let Some(bytecode) = self.before_task_hook {
            run_before_hook(bytecode, &self.task.name, &raw_description)
        } else {
            raw_description
        };

        let mut messages = Vec::new();
        let mut total_usage = TaskTokenUsage::default();

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
            .map(|s| interpolate(s, self.completed_results));
        let context = self
            .task
            .context
            .as_ref()
            .map(|s| interpolate(s, self.completed_results));

        // Build user prompt with interpolated context
        let mut prompt_parts = vec![format!("Task: {}", description)];

        if let Some(ref expected) = expected_output {
            prompt_parts.push(format!("Expected output: {}", expected));
        }

        if let Some(ref ctx) = context {
            prompt_parts.push(format!("Additional context: {}", ctx));
        }

        // Inject memory context if available
        if !self.memory_context.is_empty() {
            prompt_parts.push(format!("Relevant memory:\n{}", self.memory_context));
        }

        // Inject messages from other agents
        if !self.messages_context.is_empty() {
            prompt_parts.push(self.messages_context.to_string());
        }

        // Inject dependency results
        for dep_name in &self.task.depends_on {
            if let Some(dep_result) = self.completed_results.get(dep_name)
                && dep_result.success
            {
                prompt_parts.push(format!("Result from '{}': {}", dep_name, dep_result.output));
            }
        }

        messages.push(ChatMessage::user(&prompt_parts.join("\n\n")));

        // Get tool schemas for this agent
        let tool_schemas = self.tool_registry.schemas_for(&self.agent.tools);
        let has_tools = !tool_schemas.is_empty();

        let mut rounds = 0;

        loop {
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
                total_usage.prompt_tokens += usage.prompt_tokens;
                total_usage.completion_tokens += usage.completion_tokens;
                total_usage.total_tokens += usage.total_tokens;
                total_usage.cached_tokens += usage.cached_tokens;
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

                return Ok((
                    final_output,
                    if has_usage { Some(total_usage) } else { None },
                ));
            }

            rounds += 1;
            if rounds > self.max_tool_rounds {
                return Err(IronCrewError::Task {
                    task: self.task.name.clone(),
                    message: format!("Exceeded max tool rounds ({})", self.max_tool_rounds),
                });
            }

            // Add assistant message with tool calls (must include the tool_calls array)
            messages.push(ChatMessage::assistant(
                response.content.clone(),
                Some(response.tool_calls.clone()),
            ));

            // Execute tool calls and add tool result messages
            for tool_call in &response.tool_calls {
                tracing::info!(
                    "Executing tool '{}' for task '{}'",
                    tool_call.function.name,
                    self.task.name
                );

                let args: serde_json::Value = serde_json::from_str(&tool_call.function.arguments)
                    .unwrap_or(serde_json::Value::Object(serde_json::Map::new()));

                let tool_timeout = std::time::Duration::from_secs(
                    std::env::var("IRONCREW_TOOL_TIMEOUT")
                        .ok()
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(60),
                );

                let tool_result = match tokio::time::timeout(
                    tool_timeout,
                    self.tool_registry.execute(&tool_call.function.name, args),
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
) -> Result<(String, Option<TaskTokenUsage>)> {
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
) -> Result<(String, Option<TaskTokenUsage>)> {
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
    };
    ctx.execute().await
}
