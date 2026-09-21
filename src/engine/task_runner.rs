use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use crate::engine::agent::Agent;
use crate::engine::executor::execute_task_standalone_with_hooks;
use crate::engine::task::{Task, TaskResult};
use crate::llm::provider::LlmProvider;
use crate::tools::registry::ToolRegistry;
use crate::usage::UsageSnapshot;
use crate::utils::error::IronCrewError;

use crate::engine::input_bridge::AskHumanContext;
use crate::engine::memory::MemoryStore;
use crate::engine::messagebus::MessageBus;

mod error_handler;
pub use error_handler::handle_task_error;

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
/// Returns `(task_name, agent_name, result, duration_ms, usage, reasoning)`.
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
    UsageSnapshot,
    Option<String>,
) {
    let tracker = match crate::llm::scope::child_scope(provider.as_ref()) {
        Ok(tracker) => tracker,
        Err(error) => {
            return (
                task.name.clone(),
                agent.name.clone(),
                Err(error),
                0,
                UsageSnapshot::unavailable(),
                None,
            );
        }
    };
    let provider = crate::llm::scope::with_usage_tracker(provider, tracker.clone());
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
    let (mut output, reasoning) = loop {
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
            Ok(Ok((out, reas, _usage))) => break (Ok(out), reas),
            Ok(Err(e)) => {
                if !e.allows_task_retry() || attempt >= max_retries {
                    break (Err(e), None);
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

    let usage = match crate::llm::scope::snapshot(&tracker) {
        Ok(usage) => usage,
        Err(error) => {
            output = Err(error);
            UsageSnapshot::unavailable()
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
        usage,
        reasoning,
    )
}
