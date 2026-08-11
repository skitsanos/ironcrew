use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use crate::engine::agent::{Agent, AgentSelector};
use crate::engine::executor::{execute_task_standalone, execute_task_standalone_with_hooks};
use crate::engine::task::{Task, TaskResult, TaskTokenUsage};
use crate::llm::provider::LlmProvider;
use crate::tools::registry::ToolRegistry;
use crate::utils::error::IronCrewError;

use crate::engine::input_bridge::AskHumanContext;
use crate::engine::memory::MemoryStore;
use crate::engine::messagebus::MessageBus;

const DEFAULT_MAX_RETRY_BACKOFF_SECS: f64 = 300.0;
const HARD_MAX_RETRY_BACKOFF_SECS: f64 = 3_600.0;

fn retry_backoff(attempt: u32, base_seconds: f64) -> std::time::Duration {
    let cap = std::env::var("IRONCREW_MAX_RETRY_BACKOFF_SECS")
        .ok()
        .and_then(|value| value.parse::<f64>().ok())
        .filter(|value| value.is_finite() && *value > 0.0 && *value <= HARD_MAX_RETRY_BACKOFF_SECS)
        .unwrap_or(DEFAULT_MAX_RETRY_BACKOFF_SECS);
    let seconds = (base_seconds * 2f64.powi(attempt.min(63) as i32)).min(cap);
    std::time::Duration::try_from_secs_f64(seconds)
        .unwrap_or_else(|_| std::time::Duration::from_secs(1))
}

/// Race `fut` against a budget that only ticks while the run is NOT
/// suspended on a human question. A task that lawfully pauses on the
/// agent-facing `ask_human` tool is observably waiting, not stuck — so
/// human-wait time is excluded from the task timeout instead of forcing
/// flow authors to inflate `timeout_secs` by the worst-case answer delay.
///
/// Granularity note: the bridge is per-run, so while ANY question is
/// pending the clock pauses for every task in the run. Coarse, but safe —
/// the run-lifetime cap (`IRONCREW_MAX_RUN_LIFETIME`) still bounds the
/// whole run.
async fn timeout_excluding_human_wait<F, T>(
    budget: std::time::Duration,
    ask_human: Option<&AskHumanContext>,
    fut: F,
) -> std::result::Result<T, tokio::time::error::Elapsed>
where
    F: Future<Output = T>,
{
    // No bridge in scope -> plain timeout, identical to the old behavior.
    let Some(ask) = ask_human else {
        return tokio::time::timeout(budget, fut).await;
    };

    tokio::pin!(fut);
    let tick = std::time::Duration::from_millis(500);
    let mut remaining = budget;
    loop {
        let slice = tick.min(remaining);
        match tokio::time::timeout(slice, &mut fut).await {
            Ok(v) => return Ok(v),
            Err(elapsed) => {
                // Only bill the slice against the budget when no human
                // question is pending.
                if ask.bridge.pending_count() == 0 {
                    remaining = remaining.saturating_sub(slice);
                    if remaining.is_zero() {
                        return Err(elapsed);
                    }
                }
            }
        }
    }
}

/// Execute a single task with retry/timeout logic inside a spawned context.
///
/// Returns `(task_name, agent_name, result, duration_ms, token_usage, reasoning)`.
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
    before_task_hook: Option<Vec<u8>>,
    after_task_hook: Option<Vec<u8>>,
    ask_human: Option<AskHumanContext>,
) -> (
    String,
    String,
    std::result::Result<String, IronCrewError>,
    u64,
    Option<TaskTokenUsage>,
    Option<String>,
) {
    let task_observation = crate::engine::task_observation::TaskObservation::start();
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
    let (output, reasoning, token_usage) = loop {
        let result = execute_task_standalone_with_hooks(
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
            None,
            None,
            before_task_hook.as_deref(),
            after_task_hook.as_deref(),
            ask_human.as_ref(),
        );
        match timeout_excluding_human_wait(timeout_dur, ask_human.as_ref(), result).await {
            Ok(Ok((out, reas, usage))) => break (Ok(out), reas, usage),
            Ok(Err(e)) => {
                if attempt >= max_retries {
                    break (Err(e), None, None);
                }
                let backoff = retry_backoff(attempt, base_backoff);
                tracing::warn!(
                    "Task '{}' failed (attempt {}/{}), retrying in {:.1}s: {}",
                    task_owned.name,
                    attempt + 1,
                    max_retries + 1,
                    backoff.as_secs_f64(),
                    e
                );
                tokio::time::sleep(backoff).await;
                attempt += 1;
            }
            Err(_) => {
                if attempt >= max_retries {
                    break (
                        Err(IronCrewError::Task {
                            task: task_owned.name.clone(),
                            message: format!("Timed out after {}s", timeout_dur.as_secs()),
                        }),
                        None,
                        None,
                    );
                }
                let backoff = retry_backoff(attempt, base_backoff);
                tracing::warn!(
                    "Task '{}' timed out (attempt {}/{}), retrying in {:.1}s",
                    task_owned.name,
                    attempt + 1,
                    max_retries + 1,
                    backoff.as_secs_f64()
                );
                tokio::time::sleep(backoff).await;
                attempt += 1;
            }
        }
    };

    let duration = start.elapsed().as_millis() as u64;
    task_observation.finish(if output.is_ok() {
        crate::metrics::TaskOutcome::Success
    } else {
        crate::metrics::TaskOutcome::Error
    });
    (
        task_owned.name.clone(),
        agent_owned.name.clone(),
        output,
        duration,
        token_usage,
        reasoning,
    )
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
        crew_agents.iter().find(|a| a.name == *ea_name).unwrap_or(
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

    let task_observation = crate::engine::task_observation::TaskObservation::start();
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
        Ok((output, reasoning, token_usage)) => {
            task_observation.finish(crate::metrics::TaskOutcome::Success);
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
                token_usage: None,
                reasoning: None,
            };
            let handler_result = TaskResult {
                task: error_handler_name.clone(),
                agent: error_agent.name.clone(),
                output,
                success: true,
                duration_ms: error_start.elapsed().as_millis() as u64,
                token_usage,
                reasoning,
            };
            Some((recovered_result, Some(handler_result)))
        }
        Err(handler_err) => {
            task_observation.finish(crate::metrics::TaskOutcome::Error);
            tracing::error!(
                "Error handler '{}' also failed: {}",
                error_handler_name,
                handler_err
            );
            None
        }
    }
}
