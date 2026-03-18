use std::collections::{HashMap, HashSet};
use std::time::Instant;

use crate::engine::agent::{Agent, AgentSelector};
use crate::engine::task::{validate_dependency_graph, topological_sort, Task, TaskResult};
use crate::llm::provider::*;
use crate::tools::registry::ToolRegistry;
use crate::utils::error::{IronCrewError, Result};

#[allow(dead_code)]
pub struct ProviderConfig {
    pub provider: String,
    pub model: String,
    pub base_url: Option<String>,
    pub api_key: Option<String>,
}

pub struct Crew {
    #[allow(dead_code)]
    pub goal: String,
    pub agents: Vec<Agent>,
    pub tasks: Vec<Task>,
    pub provider_config: ProviderConfig,
    pub max_tool_rounds: usize,
}

impl Crew {
    pub fn new(goal: String, provider_config: ProviderConfig) -> Self {
        Self {
            goal,
            agents: Vec::new(),
            tasks: Vec::new(),
            provider_config,
            max_tool_rounds: 10,
        }
    }

    pub fn add_agent(&mut self, agent: Agent) {
        self.agents.push(agent);
    }

    pub fn add_task(&mut self, task: Task) {
        self.tasks.push(task);
    }

    pub async fn run(
        &self,
        provider: &dyn LlmProvider,
        tool_registry: &ToolRegistry,
    ) -> Result<Vec<TaskResult>> {
        if self.agents.is_empty() {
            return Err(IronCrewError::Validation("No agents in crew".into()));
        }
        if self.tasks.is_empty() {
            return Err(IronCrewError::Validation("No tasks in crew".into()));
        }

        validate_dependency_graph(&self.tasks)?;
        let sorted_tasks = topological_sort(&self.tasks);

        let mut results: HashMap<String, TaskResult> = HashMap::new();
        let mut failed_tasks: HashSet<String> = HashSet::new();

        // Collect error handler task names so we can skip them in normal execution
        let error_handler_names: HashSet<&str> = self
            .tasks
            .iter()
            .filter_map(|t| t.on_error.as_deref())
            .collect();

        for task in &sorted_tasks {
            // Skip error handler tasks — they run only when triggered
            if error_handler_names.contains(task.name.as_str()) {
                continue;
            }
            // Check if any dependency failed
            let dep_failed = task.depends_on.iter().find(|dep| failed_tasks.contains(*dep));
            if let Some(failed_dep) = dep_failed {
                let result = TaskResult {
                    task: task.name.clone(),
                    agent: String::new(),
                    output: format!("Skipped: dependency '{}' failed", failed_dep),
                    success: false,
                    duration_ms: 0,
                };
                failed_tasks.insert(task.name.clone());
                results.insert(task.name.clone(), result);
                tracing::warn!("Skipping task '{}': dependency '{}' failed", task.name, failed_dep);
                continue;
            }

            // Check condition if present
            if let Some(ref condition) = task.condition {
                let should_run = self.evaluate_condition(condition, &results);
                if !should_run {
                    let result = TaskResult {
                        task: task.name.clone(),
                        agent: String::new(),
                        output: format!(
                            "Skipped: condition '{}' evaluated to false",
                            condition
                        ),
                        success: true,
                        duration_ms: 0,
                    };
                    results.insert(task.name.clone(), result);
                    tracing::info!(
                        "Skipping task '{}': condition '{}' is false",
                        task.name,
                        condition
                    );
                    continue;
                }
            }

            // Select agent
            let agent = if let Some(ref agent_name) = task.agent {
                self.agents
                    .iter()
                    .find(|a| a.name == *agent_name)
                    .ok_or_else(|| IronCrewError::Validation(format!(
                        "Task '{}' assigned to unknown agent '{}'",
                        task.name, agent_name
                    )))?
            } else {
                AgentSelector::select(&self.agents, task)
            };

            tracing::info!("Task '{}' assigned to agent '{}'", task.name, agent.name);

            let start = Instant::now();

            let max_retries = task.max_retries.unwrap_or(0);
            let base_backoff = task.retry_backoff_secs.unwrap_or(1.0);
            let timeout_duration = task
                .timeout_secs
                .map(std::time::Duration::from_secs)
                .unwrap_or(std::time::Duration::from_secs(300));

            let mut attempt = 0u32;
            let output = loop {
                let task_future =
                    self.execute_task(task, agent, provider, tool_registry, &results);

                match tokio::time::timeout(timeout_duration, task_future).await {
                    Ok(Ok(output)) => break Ok(output),
                    Ok(Err(e)) => {
                        if attempt >= max_retries {
                            break Err(e);
                        }
                        let backoff = base_backoff * 2f64.powi(attempt as i32);
                        tracing::warn!(
                            "Task '{}' failed (attempt {}/{}), retrying in {:.1}s: {}",
                            task.name,
                            attempt + 1,
                            max_retries + 1,
                            backoff,
                            e
                        );
                        tokio::time::sleep(std::time::Duration::from_secs_f64(backoff)).await;
                        attempt += 1;
                    }
                    Err(_) => {
                        let msg =
                            format!("Timed out after {}s", timeout_duration.as_secs());
                        if attempt >= max_retries {
                            break Err(IronCrewError::Task {
                                task: task.name.clone(),
                                message: msg,
                            });
                        }
                        let backoff = base_backoff * 2f64.powi(attempt as i32);
                        tracing::warn!(
                            "Task '{}' timed out (attempt {}/{}), retrying in {:.1}s",
                            task.name,
                            attempt + 1,
                            max_retries + 1,
                            backoff
                        );
                        tokio::time::sleep(std::time::Duration::from_secs_f64(backoff)).await;
                        attempt += 1;
                    }
                }
            };

            match output {
                Ok(output) => {
                    let result = TaskResult {
                        task: task.name.clone(),
                        agent: agent.name.clone(),
                        output,
                        success: true,
                        duration_ms: start.elapsed().as_millis() as u64,
                    };
                    tracing::info!(
                        "Task '{}' completed in {}ms",
                        task.name,
                        result.duration_ms
                    );
                    results.insert(task.name.clone(), result);
                }
                Err(e) => {
                    let error_msg = e.to_string();

                    // Check if this task has an on_error handler
                    if let Some(ref error_handler_name) = task.on_error {
                        tracing::info!(
                            "Task '{}' failed, routing to error handler '{}'",
                            task.name,
                            error_handler_name
                        );

                        if let Some(error_handler) =
                            self.tasks.iter().find(|t| t.name == *error_handler_name)
                        {
                            let mut error_task = error_handler.clone();
                            let error_context = format!(
                                "Error from task '{}' (agent: {}): {}",
                                task.name, agent.name, error_msg
                            );
                            error_task.context = Some(
                                error_task
                                    .context
                                    .as_ref()
                                    .map_or(error_context.clone(), |existing| {
                                        format!("{}\n\n{}", existing, error_context)
                                    }),
                            );

                            let error_agent =
                                if let Some(ref agent_name) = error_task.agent {
                                    self.agents
                                        .iter()
                                        .find(|a| a.name == *agent_name)
                                        .unwrap_or(agent)
                                } else {
                                    AgentSelector::select(&self.agents, &error_task)
                                };

                            let error_start = Instant::now();
                            match self
                                .execute_task(
                                    &error_task,
                                    error_agent,
                                    provider,
                                    tool_registry,
                                    &results,
                                )
                                .await
                            {
                                Ok(output) => {
                                    tracing::info!(
                                        "Error handler '{}' succeeded, task '{}' recovered",
                                        error_handler_name,
                                        task.name
                                    );
                                    let result = TaskResult {
                                        task: task.name.clone(),
                                        agent: agent.name.clone(),
                                        output: format!(
                                            "Recovered via '{}': {}",
                                            error_handler_name, output
                                        ),
                                        success: true,
                                        duration_ms: start.elapsed().as_millis() as u64,
                                    };
                                    results.insert(task.name.clone(), result);
                                    let handler_result = TaskResult {
                                        task: error_handler_name.clone(),
                                        agent: error_agent.name.clone(),
                                        output,
                                        success: true,
                                        duration_ms: error_start.elapsed().as_millis() as u64,
                                    };
                                    results.insert(error_handler_name.clone(), handler_result);
                                    continue;
                                }
                                Err(handler_err) => {
                                    tracing::error!(
                                        "Error handler '{}' also failed: {}",
                                        error_handler_name,
                                        handler_err
                                    );
                                }
                            }
                        } else {
                            tracing::warn!(
                                "on_error handler '{}' not found for task '{}'",
                                error_handler_name,
                                task.name
                            );
                        }
                    }

                    // Original failure path (no handler or handler failed)
                    let result = TaskResult {
                        task: task.name.clone(),
                        agent: agent.name.clone(),
                        output: error_msg,
                        success: false,
                        duration_ms: start.elapsed().as_millis() as u64,
                    };
                    tracing::error!("Task '{}' failed: {}", task.name, e);
                    failed_tasks.insert(task.name.clone());
                    results.insert(task.name.clone(), result);
                }
            }
        }

        // Mark untriggered error handler tasks as skipped
        let all_error_handler_names: HashSet<String> = self
            .tasks
            .iter()
            .filter_map(|t| t.on_error.clone())
            .collect();
        for handler_name in &all_error_handler_names {
            if !results.contains_key(handler_name) {
                results.insert(
                    handler_name.clone(),
                    TaskResult {
                        task: handler_name.clone(),
                        agent: String::new(),
                        output: "Skipped: error handler not triggered".into(),
                        success: true,
                        duration_ms: 0,
                    },
                );
            }
        }

        // Return results in sorted order
        Ok(sorted_tasks
            .iter()
            .filter_map(|t| results.remove(&t.name))
            .collect())
    }

    fn evaluate_condition(
        &self,
        condition: &str,
        results: &HashMap<String, TaskResult>,
    ) -> bool {
        let lua = mlua::Lua::new();

        let Ok(ctx) = lua.create_table() else {
            return false;
        };
        for (name, result) in results {
            let Ok(entry) = lua.create_table() else {
                continue;
            };
            let _ = entry.set("output", result.output.clone());
            let _ = entry.set("success", result.success);
            let _ = entry.set("agent", result.agent.clone());
            let _ = ctx.set(name.as_str(), entry);
        }
        let _ = lua.globals().set("results", ctx);

        match lua.load(condition).eval::<mlua::Value>() {
            Ok(mlua::Value::Boolean(b)) => b,
            Ok(mlua::Value::Nil) => false,
            Ok(_) => true,
            Err(e) => {
                tracing::warn!(
                    "Condition evaluation failed for '{}': {}",
                    condition,
                    e
                );
                false
            }
        }
    }

    async fn execute_task(
        &self,
        task: &Task,
        agent: &Agent,
        provider: &dyn LlmProvider,
        tool_registry: &ToolRegistry,
        completed_results: &HashMap<String, TaskResult>,
    ) -> Result<String> {
        let mut messages = Vec::new();

        // System prompt
        let system_content = agent
            .system_prompt
            .clone()
            .unwrap_or_else(|| format!("You are {}. Your goal: {}", agent.name, agent.goal));
        messages.push(ChatMessage::system(&system_content));

        // Build user prompt with context
        let mut prompt_parts = vec![format!("Task: {}", task.description)];

        if let Some(ref expected) = task.expected_output {
            prompt_parts.push(format!("Expected output: {}", expected));
        }

        if let Some(ref context) = task.context {
            prompt_parts.push(format!("Additional context: {}", context));
        }

        // Inject dependency results
        for dep_name in &task.depends_on {
            if let Some(dep_result) = completed_results.get(dep_name)
                && dep_result.success
            {
                prompt_parts.push(format!(
                    "Result from '{}': {}",
                    dep_name, dep_result.output
                ));
            }
        }

        messages.push(ChatMessage::user(&prompt_parts.join("\n\n")));

        let model = agent
            .model
            .clone()
            .unwrap_or_else(|| self.provider_config.model.clone());

        // Get tool schemas for this agent
        let tool_schemas = tool_registry.schemas_for(&agent.tools);
        let has_tools = !tool_schemas.is_empty();

        let mut rounds = 0;

        loop {
            let request = ChatRequest {
                messages: messages.clone(),
                model: model.clone(),
                temperature: agent.temperature,
                max_tokens: agent.max_tokens,
                response_format: agent.response_format.clone(),
            };

            let response = if has_tools {
                provider.chat_with_tools(request, &tool_schemas).await?
            } else {
                provider.chat(request).await?
            };

            // If no tool calls, return the content
            if response.tool_calls.is_empty() {
                return response
                    .content
                    .ok_or_else(|| IronCrewError::Provider("Empty response from LLM".into()));
            }

            rounds += 1;
            if rounds > self.max_tool_rounds {
                return Err(IronCrewError::Task {
                    task: task.name.clone(),
                    message: format!(
                        "Exceeded max tool rounds ({})",
                        self.max_tool_rounds
                    ),
                });
            }

            // Add assistant message with tool calls (must include the tool_calls array)
            messages.push(ChatMessage::assistant(
                response.content.clone(),
                Some(response.tool_calls.clone()),
            ));

            // Execute tool calls and add tool result messages
            for tool_call in &response.tool_calls {
                tracing::info!("Executing tool '{}' for task '{}'", tool_call.function.name, task.name);

                let args: serde_json::Value = serde_json::from_str(&tool_call.function.arguments)
                    .unwrap_or(serde_json::Value::Object(serde_json::Map::new()));

                let tool_result = tool_registry
                    .execute(&tool_call.function.name, args)
                    .await;

                let result_text = match tool_result {
                    Ok(output) => output,
                    Err(e) => format!("Tool error: {}", e),
                };

                messages.push(ChatMessage::tool(&tool_call.id, &result_text));
            }
        }
    }
}
