use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use crate::engine::agent::{Agent, AgentSelector};
use crate::engine::executor::execute_task_standalone;
use crate::engine::task::{Task, TaskResult};
use crate::llm::provider::LlmProvider;
use crate::tools::registry::ToolRegistry;
use crate::utils::error::IronCrewError;

use crate::engine::memory::MemoryStore;
use crate::engine::messagebus::MessageBus;

/// Execute a single task with retry/timeout logic inside a spawned context.
///
/// Returns `(task_name, agent_name, result, duration_ms)`.
#[allow(clippy::too_many_arguments)]
pub async fn run_single_task(
    task: &Task,
    agent: &Agent,
    provider: Arc<dyn LlmProvider>,
    tool_registry: ToolRegistry,
    results_snapshot: HashMap<String, TaskResult>,
    model: String,
    max_tool_rounds: usize,
    memory: &MemoryStore,
    messagebus: &MessageBus,
    should_stream: bool,
) -> (
    String,
    String,
    std::result::Result<String, IronCrewError>,
    u64,
) {
    // Build memory context for this task
    let memory_context = memory.build_context(&task.description, 5).await;

    // Collect pending messages for this agent
    let pending_messages = messagebus.receive(&agent.name).await;
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
    let task_owned = task.clone();
    let agent_owned = agent.clone();

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
            provider.as_ref(),
            &tool_registry,
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
                tokio::time::sleep(std::time::Duration::from_secs_f64(backoff)).await;
                attempt += 1;
            }
            Err(_) => {
                if attempt >= max_retries {
                    break Err(IronCrewError::Task {
                        task: task_owned.name.clone(),
                        message: format!("Timed out after {}s", timeout_dur.as_secs()),
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
                tokio::time::sleep(std::time::Duration::from_secs_f64(backoff)).await;
                attempt += 1;
            }
        }
    };

    let duration = start.elapsed().as_millis() as u64;
    (task_owned.name.clone(), agent_owned.name.clone(), output, duration)
}

/// Handle a task error by running the on_error handler task if one is configured.
///
/// Returns `Some((recovered_result, handler_result))` if the error was handled successfully.
/// Returns `None` if no handler was found or the handler itself failed.
#[allow(clippy::too_many_arguments)]
pub async fn handle_task_error(
    task: &Task,
    agent_name: &str,
    error_msg: &str,
    crew_tasks: &[Task],
    crew_agents: &[Agent],
    provider: Arc<dyn LlmProvider>,
    tool_registry: &ToolRegistry,
    results: &HashMap<String, TaskResult>,
    memory: &MemoryStore,
    model: &str,
    max_tool_rounds: usize,
) -> Option<(TaskResult, Option<TaskResult>)> {
    let error_handler_name = task.on_error.as_ref()?;

    tracing::info!(
        "Task '{}' failed, routing to error handler '{}'",
        task.name,
        error_handler_name
    );

    let error_handler = crew_tasks.iter().find(|t| t.name == *error_handler_name);
    let error_handler = match error_handler {
        Some(h) => h,
        None => {
            tracing::warn!(
                "on_error handler '{}' not found for task '{}'",
                error_handler_name,
                task.name
            );
            return None;
        }
    };

    let mut error_task = error_handler.clone();
    let error_context = format!(
        "Error from task '{}' (agent: {}): {}",
        task.name, agent_name, error_msg
    );
    error_task.context = Some(
        error_task
            .context
            .as_ref()
            .map_or(error_context.clone(), |existing| {
                format!("{}\n\n{}", existing, error_context)
            }),
    );

    let error_agent = if let Some(ref ea_name) = error_task.agent {
        crew_agents
            .iter()
            .find(|a| a.name == *ea_name)
            .unwrap_or(
                crew_agents
                    .iter()
                    .find(|a| a.name == agent_name)
                    .unwrap_or(&crew_agents[0]),
            )
    } else {
        AgentSelector::select(crew_agents, &error_task)
    };

    let error_model = error_agent
        .model
        .clone()
        .unwrap_or_else(|| model.to_string());
    let error_start = Instant::now();

    // Provide empty memory_context placeholder (consistent with original)
    let _memory = memory;

    match execute_task_standalone(
        &error_task,
        error_agent,
        provider.as_ref(),
        tool_registry,
        results,
        &error_model,
        max_tool_rounds,
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
                task.name
            );
            let recovered_result = TaskResult {
                task: task.name.clone(),
                agent: agent_name.to_string(),
                output: format!("Recovered via '{}': {}", error_handler_name, output),
                success: true,
                duration_ms: 0, // caller sets actual duration
            };
            let handler_result = TaskResult {
                task: error_handler_name.clone(),
                agent: error_agent.name.clone(),
                output,
                success: true,
                duration_ms: error_start.elapsed().as_millis() as u64,
            };
            Some((recovered_result, Some(handler_result)))
        }
        Err(handler_err) => {
            tracing::error!(
                "Error handler '{}' also failed: {}",
                error_handler_name,
                handler_err
            );
            None
        }
    }
}
