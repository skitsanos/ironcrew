use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;

use crate::engine::agent::{Agent, AgentSelector};
use crate::engine::interpolate::interpolate;
use crate::engine::memory::MemoryStore;
use crate::engine::messagebus::MessageBus;
use crate::engine::task::{validate_dependency_graph, topological_phases, Task, TaskResult};
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
    pub max_concurrent_tasks: Option<usize>,
    pub memory: MemoryStore,
    pub messagebus: MessageBus,
}

impl Crew {
    pub fn new(goal: String, provider_config: ProviderConfig, memory: MemoryStore) -> Self {
        Self {
            goal,
            agents: Vec::new(),
            tasks: Vec::new(),
            provider_config,
            max_tool_rounds: 10,
            max_concurrent_tasks: None,
            memory,
            messagebus: MessageBus::new(),
        }
    }

    pub fn add_agent(&mut self, agent: Agent) {
        self.agents.push(agent);
    }

    pub fn add_task(&mut self, task: Task) {
        self.tasks.push(task);
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn execute_collaborative_task(
        &self,
        task_description: &str,
        agent_names: &[String],
        max_turns: usize,
        provider: Arc<dyn LlmProvider>,
        _tool_registry: &ToolRegistry,
        completed_results: &HashMap<String, TaskResult>,
        memory_context: &str,
    ) -> Result<String> {
        // Resolve agents
        let agents: Vec<&Agent> = agent_names
            .iter()
            .filter_map(|name| self.agents.iter().find(|a| a.name == *name))
            .collect();

        if agents.len() < 2 {
            return Err(IronCrewError::Validation(
                "Collaborative task requires at least 2 agents".into(),
            ));
        }

        let model = self.provider_config.model.clone();

        // Build conversation history shared across all agents
        let mut conversation: Vec<String> = Vec::new();
        conversation.push(format!("Task: {}", task_description));

        if !memory_context.is_empty() {
            conversation.push(format!("Context:\n{}", memory_context));
        }

        // Add dependency results as context
        for (name, result) in completed_results {
            if result.success {
                conversation.push(format!("Result from '{}': {}", name, result.output));
            }
        }

        for turn in 0..max_turns {
            // Each agent takes a turn
            for agent in &agents {
                let system_prompt = agent.system_prompt.clone().unwrap_or_else(|| {
                    format!(
                        "You are {} in a collaborative discussion with other agents. Your goal: {}. \
                         Build on what others have said. Be concise and constructive.",
                        agent.name, agent.goal
                    )
                });

                let mut messages = vec![ChatMessage::system(&system_prompt)];

                // Add the conversation so far
                let conversation_text = conversation.join("\n\n");
                let user_prompt = if turn == 0 && conversation.len() <= 1 {
                    format!(
                        "{}\n\nYou are starting the discussion. Share your initial thoughts.",
                        conversation_text
                    )
                } else {
                    format!(
                        "{}\n\nIt's your turn. Respond to the discussion, adding your perspective.",
                        conversation_text
                    )
                };
                messages.push(ChatMessage::user(&user_prompt));

                let agent_model = agent.model.clone().unwrap_or_else(|| model.clone());

                let request = ChatRequest {
                    messages,
                    model: agent_model,
                    temperature: agent.temperature,
                    max_tokens: agent.max_tokens,
                    response_format: agent.response_format.clone(),
                };

                let response = provider.chat(request).await?;
                let content = response.content.unwrap_or_default();

                conversation.push(format!("[{}]: {}", agent.name, content));

                tracing::info!(
                    "Collaborative task turn {}, agent '{}' responded",
                    turn + 1,
                    agent.name
                );
            }
        }

        // Final synthesis: ask the first agent to summarize
        let synth_agent = agents[0];
        let system_prompt = format!(
            "You are {}. Synthesize the collaborative discussion into a final, cohesive response.",
            synth_agent.name
        );
        let conversation_text = conversation.join("\n\n");

        let request = ChatRequest {
            messages: vec![
                ChatMessage::system(&system_prompt),
                ChatMessage::user(&format!(
                    "Here is the full discussion:\n\n{}\n\nProvide a final synthesized response that combines the best insights from all participants.",
                    conversation_text
                )),
            ],
            model: synth_agent.model.clone().unwrap_or_else(|| model.clone()),
            temperature: synth_agent.temperature,
            max_tokens: synth_agent.max_tokens,
            response_format: synth_agent.response_format.clone(),
        };

        let response = provider.chat(request).await?;
        response
            .content
            .ok_or_else(|| IronCrewError::Provider("Empty synthesis response".into()))
    }

    pub async fn run(
        &self,
        provider: Arc<dyn LlmProvider>,
        tool_registry: &ToolRegistry,
    ) -> Result<Vec<TaskResult>> {
        if self.agents.is_empty() {
            return Err(IronCrewError::Validation("No agents in crew".into()));
        }
        if self.tasks.is_empty() {
            return Err(IronCrewError::Validation("No tasks in crew".into()));
        }

        // Register all agents in the messagebus
        for agent in &self.agents {
            self.messagebus.register_agent(&agent.name).await;
        }

        validate_dependency_graph(&self.tasks)?;
        let phases = topological_phases(&self.tasks);

        let mut results: HashMap<String, TaskResult> = HashMap::new();
        let mut failed_tasks: HashSet<String> = HashSet::new();

        // Collect error handler task names so we can skip them in normal execution
        let error_handler_names: HashSet<&str> = self
            .tasks
            .iter()
            .filter_map(|t| t.on_error.as_deref())
            .collect();

        let semaphore = self
            .max_concurrent_tasks
            .map(|n| Arc::new(tokio::sync::Semaphore::new(n)));

        // Build a flat ordering of task names for final result ordering
        let task_order: Vec<&str> = phases
            .iter()
            .flat_map(|phase| phase.iter().map(|t| t.name.as_str()))
            .collect();

        for (phase_idx, phase) in phases.iter().enumerate() {
            // Filter eligible tasks for this phase
            let mut phase_tasks: Vec<&Task> = Vec::new();

            for task in phase {
                // Skip error handler tasks -- they run only when triggered
                if error_handler_names.contains(task.name.as_str()) {
                    continue;
                }

                // Check if any dependency failed
                if let Some(failed_dep) =
                    task.depends_on.iter().find(|d| failed_tasks.contains(*d))
                {
                    let result = TaskResult {
                        task: task.name.clone(),
                        agent: String::new(),
                        output: format!("Skipped: dependency '{}' failed", failed_dep),
                        success: false,
                        duration_ms: 0,
                    };
                    failed_tasks.insert(task.name.clone());
                    results.insert(task.name.clone(), result);
                    tracing::warn!(
                        "Skipping task '{}': dependency '{}' failed",
                        task.name,
                        failed_dep
                    );
                    continue;
                }

                // Check condition if present
                if let Some(ref condition) = task.condition {
                    let interpolated_condition = interpolate(condition, &results);
                    let should_run =
                        evaluate_condition(&interpolated_condition, &results);
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

                phase_tasks.push(task);
            }

            if phase_tasks.is_empty() {
                continue;
            }

            tracing::info!(
                "Phase {}: executing {} task(s) in parallel: [{}]",
                phase_idx,
                phase_tasks.len(),
                phase_tasks
                    .iter()
                    .map(|t| t.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            );

            // Handle collaborative tasks first (they run sequentially)
            let mut standard_tasks: Vec<&Task> = Vec::new();
            for task in &phase_tasks {
                if task.task_type.as_deref() == Some("collaborative")
                    && task.collaborative_agents.len() >= 2
                {
                    tracing::info!(
                        "Running collaborative task '{}' with agents: [{}]",
                        task.name,
                        task.collaborative_agents.join(", ")
                    );

                    let memory_context = self.memory.build_context(&task.description, 5).await;
                    let max_turns = task.max_turns.unwrap_or(3);

                    let start = Instant::now();
                    match self
                        .execute_collaborative_task(
                            &interpolate(&task.description, &results),
                            &task.collaborative_agents,
                            max_turns,
                            provider.clone(),
                            tool_registry,
                            &results,
                            &memory_context,
                        )
                        .await
                    {
                        Ok(output) => {
                            let duration_ms = start.elapsed().as_millis() as u64;
                            tracing::info!(
                                "Collaborative task '{}' completed in {}ms",
                                task.name,
                                duration_ms
                            );
                            results.insert(
                                task.name.clone(),
                                TaskResult {
                                    task: task.name.clone(),
                                    agent: task.collaborative_agents.join("+"),
                                    output,
                                    success: true,
                                    duration_ms,
                                },
                            );
                        }
                        Err(e) => {
                            let duration_ms = start.elapsed().as_millis() as u64;
                            let error_msg = e.to_string();

                            // Check for on_error handler
                            if let Some(ref error_handler_name) = task.on_error {
                                tracing::info!(
                                    "Collaborative task '{}' failed, routing to error handler '{}'",
                                    task.name,
                                    error_handler_name
                                );
                                if let Some(error_handler) =
                                    self.tasks.iter().find(|t| t.name == *error_handler_name)
                                {
                                    let mut error_task = error_handler.clone();
                                    let error_context = format!(
                                        "Error from collaborative task '{}': {}",
                                        task.name, error_msg
                                    );
                                    error_task.context = Some(
                                        error_task.context.as_ref().map_or(
                                            error_context.clone(),
                                            |existing| {
                                                format!("{}\n\n{}", existing, error_context)
                                            },
                                        ),
                                    );

                                    let error_agent =
                                        if let Some(ref ea_name) = error_task.agent {
                                            self.agents
                                                .iter()
                                                .find(|a| a.name == *ea_name)
                                                .unwrap_or(&self.agents[0])
                                        } else {
                                            AgentSelector::select(&self.agents, &error_task)
                                        };

                                    let error_model = error_agent
                                        .model
                                        .clone()
                                        .unwrap_or_else(|| self.provider_config.model.clone());
                                    let error_start = Instant::now();
                                    match execute_task_standalone(
                                        &error_task,
                                        error_agent,
                                        provider.as_ref(),
                                        tool_registry,
                                        &results,
                                        &error_model,
                                        self.max_tool_rounds,
                                        "",
                                        "",
                                    )
                                    .await
                                    {
                                        Ok(output) => {
                                            results.insert(
                                                task.name.clone(),
                                                TaskResult {
                                                    task: task.name.clone(),
                                                    agent: task
                                                        .collaborative_agents
                                                        .join("+"),
                                                    output: format!(
                                                        "Recovered via '{}': {}",
                                                        error_handler_name, output
                                                    ),
                                                    success: true,
                                                    duration_ms,
                                                },
                                            );
                                            results.insert(
                                                error_handler_name.clone(),
                                                TaskResult {
                                                    task: error_handler_name.clone(),
                                                    agent: error_agent.name.clone(),
                                                    output,
                                                    success: true,
                                                    duration_ms: error_start
                                                        .elapsed()
                                                        .as_millis()
                                                        as u64,
                                                },
                                            );
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
                                }
                            }

                            tracing::error!("Collaborative task '{}' failed: {}", task.name, e);
                            failed_tasks.insert(task.name.clone());
                            results.insert(
                                task.name.clone(),
                                TaskResult {
                                    task: task.name.clone(),
                                    agent: task.collaborative_agents.join("+"),
                                    output: error_msg,
                                    success: false,
                                    duration_ms,
                                },
                            );
                        }
                    }
                } else {
                    standard_tasks.push(task);
                }
            }

            // Spawn all standard tasks in this phase concurrently
            let mut handles = Vec::new();

            for task in &standard_tasks {
                // Select agent
                let agent = if let Some(ref agent_name) = task.agent {
                    self.agents
                        .iter()
                        .find(|a| a.name == *agent_name)
                        .ok_or_else(|| {
                            IronCrewError::Validation(format!(
                                "Task '{}' assigned to unknown agent '{}'",
                                task.name, agent_name
                            ))
                        })?
                } else {
                    AgentSelector::select(&self.agents, task)
                };

                tracing::info!("Task '{}' assigned to agent '{}'", task.name, agent.name);

                // Build memory context for this task
                let memory_context = self.memory.build_context(&task.description, 5).await;

                // Collect pending messages for this agent
                let pending_messages = self.messagebus.receive(&agent.name).await;
                let messages_context = if pending_messages.is_empty() {
                    String::new()
                } else {
                    let msg_strs: Vec<String> = pending_messages
                        .iter()
                        .map(|m| {
                            format!(
                                "[Message from {} ({:?})]: {}",
                                m.from, m.message_type, m.content
                            )
                        })
                        .collect();
                    format!("Messages from other agents:\n{}", msg_strs.join("\n"))
                };

                // Clone everything needed for the spawned task
                let task_owned = (*task).clone();
                let agent_owned = agent.clone();
                let provider_clone = provider.clone();
                let tool_registry_clone = tool_registry.clone();
                let results_snapshot = results.clone();
                let model = agent
                    .model
                    .clone()
                    .unwrap_or_else(|| self.provider_config.model.clone());
                let max_tool_rounds = self.max_tool_rounds;
                let sem = semaphore.clone();

                let handle = tokio::spawn(async move {
                    let _permit = match sem {
                        Some(ref s) => Some(s.acquire().await.unwrap()),
                        None => None,
                    };

                    let start = Instant::now();
                    let max_retries = task_owned.max_retries.unwrap_or(0);
                    let base_backoff = task_owned.retry_backoff_secs.unwrap_or(1.0);
                    let timeout_dur = task_owned
                        .timeout_secs
                        .map(std::time::Duration::from_secs)
                        .unwrap_or(std::time::Duration::from_secs(300));

                    let mut attempt = 0u32;
                    let output = loop {
                        let result = execute_task_standalone(
                            &task_owned,
                            &agent_owned,
                            provider_clone.as_ref(),
                            &tool_registry_clone,
                            &results_snapshot,
                            &model,
                            max_tool_rounds,
                            &memory_context,
                            &messages_context,
                        );
                        match tokio::time::timeout(timeout_dur, result).await {
                            Ok(Ok(out)) => break Ok(out),
                            Ok(Err(e)) => {
                                if attempt >= max_retries {
                                    break Err(e);
                                }
                                let backoff = base_backoff * 2f64.powi(attempt as i32);
                                tracing::warn!(
                                    "Task '{}' failed (attempt {}/{}), retrying in {:.1}s: {}",
                                    task_owned.name,
                                    attempt + 1,
                                    max_retries + 1,
                                    backoff,
                                    e
                                );
                                tokio::time::sleep(std::time::Duration::from_secs_f64(backoff))
                                    .await;
                                attempt += 1;
                            }
                            Err(_) => {
                                if attempt >= max_retries {
                                    break Err(IronCrewError::Task {
                                        task: task_owned.name.clone(),
                                        message: format!(
                                            "Timed out after {}s",
                                            timeout_dur.as_secs()
                                        ),
                                    });
                                }
                                let backoff = base_backoff * 2f64.powi(attempt as i32);
                                tracing::warn!(
                                    "Task '{}' timed out (attempt {}/{}), retrying in {:.1}s",
                                    task_owned.name,
                                    attempt + 1,
                                    max_retries + 1,
                                    backoff
                                );
                                tokio::time::sleep(std::time::Duration::from_secs_f64(backoff))
                                    .await;
                                attempt += 1;
                            }
                        }
                    };

                    let duration = start.elapsed().as_millis() as u64;
                    (task_owned.name.clone(), agent_owned.name.clone(), output, duration)
                });

                handles.push(handle);
            }

            // Await all handles in this phase
            let phase_results = futures::future::join_all(handles).await;

            // Process results
            for join_result in phase_results {
                let (task_name, agent_name, output, duration_ms) = join_result.map_err(|e| {
                    IronCrewError::Task {
                        task: "unknown".into(),
                        message: format!("Task panicked: {}", e),
                    }
                })?;

                match output {
                    Ok(out) => {
                        let result = TaskResult {
                            task: task_name.clone(),
                            agent: agent_name,
                            output: out,
                            success: true,
                            duration_ms,
                        };
                        tracing::info!("Task '{}' completed in {}ms", task_name, duration_ms);
                        results.insert(task_name, result);
                    }
                    Err(e) => {
                        let error_msg = e.to_string();

                        // Check if this task has an on_error handler
                        let task_def = self.tasks.iter().find(|t| t.name == task_name);
                        if let Some(task_def) = task_def
                            && let Some(ref error_handler_name) = task_def.on_error
                        {
                                tracing::info!(
                                    "Task '{}' failed, routing to error handler '{}'",
                                    task_name,
                                    error_handler_name
                                );

                                if let Some(error_handler) =
                                    self.tasks.iter().find(|t| t.name == *error_handler_name)
                                {
                                    let mut error_task = error_handler.clone();
                                    let error_context = format!(
                                        "Error from task '{}' (agent: {}): {}",
                                        task_name, agent_name, error_msg
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
                                        if let Some(ref ea_name) = error_task.agent {
                                            self.agents
                                                .iter()
                                                .find(|a| a.name == *ea_name)
                                                .unwrap_or(
                                                    self.agents
                                                        .iter()
                                                        .find(|a| a.name == agent_name)
                                                        .unwrap_or(&self.agents[0]),
                                                )
                                        } else {
                                            AgentSelector::select(&self.agents, &error_task)
                                        };

                                    let error_model = error_agent
                                        .model
                                        .clone()
                                        .unwrap_or_else(|| self.provider_config.model.clone());
                                    let error_start = Instant::now();
                                    match execute_task_standalone(
                                        &error_task,
                                        error_agent,
                                        provider.as_ref(),
                                        tool_registry,
                                        &results,
                                        &error_model,
                                        self.max_tool_rounds,
                                        "",
                                        "",
                                    )
                                    .await
                                    {
                                        Ok(output) => {
                                            tracing::info!(
                                                "Error handler '{}' succeeded, task '{}' recovered",
                                                error_handler_name,
                                                task_name
                                            );
                                            let result = TaskResult {
                                                task: task_name.clone(),
                                                agent: agent_name.clone(),
                                                output: format!(
                                                    "Recovered via '{}': {}",
                                                    error_handler_name, output
                                                ),
                                                success: true,
                                                duration_ms,
                                            };
                                            results.insert(task_name, result);
                                            let handler_result = TaskResult {
                                                task: error_handler_name.clone(),
                                                agent: error_agent.name.clone(),
                                                output,
                                                success: true,
                                                duration_ms: error_start.elapsed().as_millis()
                                                    as u64,
                                            };
                                            results
                                                .insert(error_handler_name.clone(), handler_result);
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
                                        task_name
                                    );
                                }
                        }

                        // Original failure path (no handler or handler failed)
                        let result = TaskResult {
                            task: task_name.clone(),
                            agent: agent_name,
                            output: error_msg,
                            success: false,
                            duration_ms,
                        };
                        tracing::error!("Task '{}' failed: {}", task_name, e);
                        failed_tasks.insert(task_name.clone());
                        results.insert(task_name, result);
                    }
                }
            }

            // Store successful task results in memory
            for (task_name, result) in &results {
                if result.success {
                    let value = serde_json::json!({
                        "output": result.output,
                        "agent": result.agent,
                        "duration_ms": result.duration_ms,
                    });
                    self.memory
                        .set(format!("task:{}", task_name), value)
                        .await;
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

        // Persist memory if using persistent backend
        self.memory.save().await.ok();

        // Return results in phase order
        Ok(task_order
            .iter()
            .filter_map(|name| results.remove(*name))
            .collect())
    }
}

fn evaluate_condition(condition: &str, results: &HashMap<String, TaskResult>) -> bool {
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

#[allow(clippy::too_many_arguments)]
async fn execute_task_standalone(
    task: &Task,
    agent: &Agent,
    provider: &dyn LlmProvider,
    tool_registry: &ToolRegistry,
    completed_results: &HashMap<String, TaskResult>,
    model: &str,
    max_tool_rounds: usize,
    memory_context: &str,
    messages_context: &str,
) -> Result<String> {
    let mut messages = Vec::new();

    // System prompt
    let system_content = agent
        .system_prompt
        .clone()
        .unwrap_or_else(|| format!("You are {}. Your goal: {}", agent.name, agent.goal));
    messages.push(ChatMessage::system(&system_content));

    // Interpolate task fields with completed results
    let description = interpolate(&task.description, completed_results);
    let expected_output = task
        .expected_output
        .as_ref()
        .map(|s| interpolate(s, completed_results));
    let context = task
        .context
        .as_ref()
        .map(|s| interpolate(s, completed_results));

    // Build user prompt with interpolated context
    let mut prompt_parts = vec![format!("Task: {}", description)];

    if let Some(ref expected) = expected_output {
        prompt_parts.push(format!("Expected output: {}", expected));
    }

    if let Some(ref ctx) = context {
        prompt_parts.push(format!("Additional context: {}", ctx));
    }

    // Inject memory context if available
    if !memory_context.is_empty() {
        prompt_parts.push(format!("Relevant memory:\n{}", memory_context));
    }

    // Inject messages from other agents
    if !messages_context.is_empty() {
        prompt_parts.push(messages_context.to_string());
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

    // Get tool schemas for this agent
    let tool_schemas = tool_registry.schemas_for(&agent.tools);
    let has_tools = !tool_schemas.is_empty();

    let mut rounds = 0;

    loop {
        let request = ChatRequest {
            messages: messages.clone(),
            model: model.to_string(),
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
        if rounds > max_tool_rounds {
            return Err(IronCrewError::Task {
                task: task.name.clone(),
                message: format!("Exceeded max tool rounds ({})", max_tool_rounds),
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
                task.name
            );

            let args: serde_json::Value =
                serde_json::from_str(&tool_call.function.arguments)
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
