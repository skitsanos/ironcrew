use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;

use crate::engine::agent::AgentSelector;
use crate::engine::collaborative::execute_collaborative_task;
use crate::engine::condition::evaluate_condition;
use crate::engine::crew::Crew;
use crate::engine::executor::execute_task_standalone;
use crate::engine::foreach::execute_foreach_task;
use crate::engine::interpolate::interpolate;
use crate::engine::task::{validate_dependency_graph, topological_phases, TaskResult};
use crate::llm::provider::LlmProvider;
use crate::tools::registry::ToolRegistry;
use crate::utils::error::{IronCrewError, Result};

pub async fn run_crew(
    crew: &Crew,
    provider: Arc<dyn LlmProvider>,
    tool_registry: &ToolRegistry,
) -> Result<Vec<TaskResult>> {
    if crew.agents.is_empty() {
        return Err(IronCrewError::Validation("No agents in crew".into()));
    }
    if crew.tasks.is_empty() {
        return Err(IronCrewError::Validation("No tasks in crew".into()));
    }

    // Register all agents in the messagebus
    for agent in &crew.agents {
        crew.messagebus.register_agent(&agent.name).await;
    }

    validate_dependency_graph(&crew.tasks)?;
    let phases = topological_phases(&crew.tasks);

    let mut results: HashMap<String, TaskResult> = HashMap::new();
    let mut failed_tasks: HashSet<String> = HashSet::new();

    // Collect error handler task names so we can skip them in normal execution
    let error_handler_names: HashSet<&str> = crew
        .tasks
        .iter()
        .filter_map(|t| t.on_error.as_deref())
        .collect();

    let semaphore = crew
        .max_concurrent_tasks
        .map(|n| Arc::new(tokio::sync::Semaphore::new(n)));

    // Build a flat ordering of task names for final result ordering
    let task_order: Vec<&str> = phases
        .iter()
        .flat_map(|phase| phase.iter().map(|t| t.name.as_str()))
        .collect();

    for (phase_idx, phase) in phases.iter().enumerate() {
        // Filter eligible tasks for this phase
        let mut phase_tasks: Vec<&crate::engine::task::Task> = Vec::new();

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

        // Handle foreach and collaborative tasks first (they run sequentially)
        let mut standard_tasks: Vec<&crate::engine::task::Task> = Vec::new();
        for task in &phase_tasks {
            // Handle foreach tasks
            if task.foreach_source.is_some() {
                // Select agent for foreach task
                let agent = if let Some(ref agent_name) = task.agent {
                    crew.agents
                        .iter()
                        .find(|a| a.name == *agent_name)
                        .ok_or_else(|| {
                            IronCrewError::Validation(format!(
                                "Task '{}' assigned to unknown agent '{}'",
                                task.name, agent_name
                            ))
                        })?
                } else {
                    AgentSelector::select(&crew.agents, task)
                };

                let model = agent
                    .model
                    .clone()
                    .unwrap_or_else(|| crew.provider_config.model.clone());

                let foreach_result = execute_foreach_task(
                    task,
                    agent,
                    provider.as_ref(),
                    tool_registry,
                    &results,
                    &crew.memory,
                    &crew.messagebus,
                    &model,
                    crew.max_tool_rounds,
                    crew.stream,
                )
                .await?;

                if !foreach_result.success {
                    // Check if the result indicates a source-not-array skip
                    if foreach_result.output.starts_with("Skipped: foreach source") && foreach_result.agent.is_empty() {
                        tracing::warn!(
                            "foreach source for task '{}' is not an array, skipping",
                            task.name
                        );
                        failed_tasks.insert(task.name.clone());
                    }
                }

                results.insert(task.name.clone(), foreach_result);
                continue; // Don't go through normal spawn path
            } else if task.task_type.as_deref() == Some("collaborative")
                && task.collaborative_agents.len() >= 2
            {
                tracing::info!(
                    "Running collaborative task '{}' with agents: [{}]",
                    task.name,
                    task.collaborative_agents.join(", ")
                );

                let memory_context = crew.memory.build_context(&task.description, 5).await;
                let max_turns = task.max_turns.unwrap_or(3);

                // Resolve agents
                let collab_agents: Vec<&crate::engine::agent::Agent> = task
                    .collaborative_agents
                    .iter()
                    .filter_map(|name| crew.agents.iter().find(|a| a.name == *name))
                    .collect();

                let model = crew.provider_config.model.clone();

                let start = Instant::now();
                match execute_collaborative_task(
                    &collab_agents,
                    &interpolate(&task.description, &results),
                    max_turns,
                    provider.clone(),
                    &results,
                    &memory_context,
                    &model,
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
                                crew.tasks.iter().find(|t| t.name == *error_handler_name)
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
                                        crew.agents
                                            .iter()
                                            .find(|a| a.name == *ea_name)
                                            .unwrap_or(&crew.agents[0])
                                    } else {
                                        AgentSelector::select(&crew.agents, &error_task)
                                    };

                                let error_model = error_agent
                                    .model
                                    .clone()
                                    .unwrap_or_else(|| crew.provider_config.model.clone());
                                let error_start = Instant::now();
                                match execute_task_standalone(
                                    &error_task,
                                    error_agent,
                                    provider.as_ref(),
                                    tool_registry,
                                    &results,
                                    &error_model,
                                    crew.max_tool_rounds,
                                    "",
                                    "",
                                    false,
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
                crew.agents
                    .iter()
                    .find(|a| a.name == *agent_name)
                    .ok_or_else(|| {
                        IronCrewError::Validation(format!(
                            "Task '{}' assigned to unknown agent '{}'",
                            task.name, agent_name
                        ))
                    })?
            } else {
                AgentSelector::select(&crew.agents, task)
            };

            tracing::info!("Task '{}' assigned to agent '{}'", task.name, agent.name);

            // Build memory context for this task
            let memory_context = crew.memory.build_context(&task.description, 5).await;

            // Collect pending messages for this agent
            let pending_messages = crew.messagebus.receive(&agent.name).await;
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
                .unwrap_or_else(|| crew.provider_config.model.clone());
            let max_tool_rounds = crew.max_tool_rounds;
            let should_stream = task.stream || crew.stream;
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
                        should_stream,
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
                    let task_def = crew.tasks.iter().find(|t| t.name == task_name);
                    if let Some(task_def) = task_def
                        && let Some(ref error_handler_name) = task_def.on_error
                    {
                            tracing::info!(
                                "Task '{}' failed, routing to error handler '{}'",
                                task_name,
                                error_handler_name
                            );

                            if let Some(error_handler) =
                                crew.tasks.iter().find(|t| t.name == *error_handler_name)
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
                                        crew.agents
                                            .iter()
                                            .find(|a| a.name == *ea_name)
                                            .unwrap_or(
                                                crew.agents
                                                    .iter()
                                                    .find(|a| a.name == agent_name)
                                                    .unwrap_or(&crew.agents[0]),
                                            )
                                    } else {
                                        AgentSelector::select(&crew.agents, &error_task)
                                    };

                                let error_model = error_agent
                                    .model
                                    .clone()
                                    .unwrap_or_else(|| crew.provider_config.model.clone());
                                let error_start = Instant::now();
                                match execute_task_standalone(
                                    &error_task,
                                    error_agent,
                                    provider.as_ref(),
                                    tool_registry,
                                    &results,
                                    &error_model,
                                    crew.max_tool_rounds,
                                    "",
                                    "",
                                    false,
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
                crew.memory
                    .set(format!("task:{}", task_name), value)
                    .await;
            }
        }
    }

    // Mark untriggered error handler tasks as skipped
    let all_error_handler_names: HashSet<String> = crew
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
    crew.memory.save().await.ok();

    // Return results in phase order
    Ok(task_order
        .iter()
        .filter_map(|name| results.remove(*name))
        .collect())
}
