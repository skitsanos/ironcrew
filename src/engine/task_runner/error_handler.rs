use super::*;
use crate::engine::agent::AgentSelector;
use crate::engine::executor::TaskExecutionContext;

/// Handle a task error by running the on_error handler task if one is configured.
///
/// Returns the handler result, including usage on failure. `None` means no handler ran.
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
    _memory: &MemoryStore,
    model: &str,
    max_tool_rounds: usize,
) -> Option<TaskResult> {
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

    let error_task = error_handler;
    let error_label = format!(
        "Previous task error (untrusted diagnostic):\nError from task '{}' (agent: {}): ",
        task.name, agent_name
    );

    let error_agent = if let Some(ref ea_name) = error_task.agent {
        crew_agents.iter().find(|a| a.name == *ea_name).unwrap_or(
            crew_agents
                .iter()
                .find(|a| a.name == agent_name)
                .unwrap_or(&crew_agents[0]),
        )
    } else {
        AgentSelector::select(crew_agents, error_task)
    };

    let error_model = error_agent
        .model
        .clone()
        .or_else(|| error_task.model.clone())
        .unwrap_or_else(|| model.to_string());
    let error_start = Instant::now();
    let tracker = match crate::llm::scope::child_scope(provider.as_ref()) {
        Ok(tracker) => tracker,
        Err(error) => {
            return Some(TaskResult {
                task: error_handler_name.clone(),
                agent: error_agent.name.clone(),
                output: error.to_string(),
                success: false,
                duration_ms: 0,
                usage: UsageSnapshot::unavailable(),
                reasoning: None,
            });
        }
    };
    let provider = crate::llm::scope::with_usage_tracker(provider, tracker.clone());
    let task_observation = crate::engine::task_observation::TaskObservation::start();
    let outcome = TaskExecutionContext {
        task: error_task,
        agent: error_agent,
        provider: provider.as_ref(),
        tool_registry,
        completed_results: results,
        model: error_model,
        max_tool_rounds,
        memory_context: String::new(),
        messages_context: String::new(),
        should_stream: false,
        prompt_cache_key: None,
        prompt_cache_retention: None,
        before_task_hook: None,
        after_task_hook: None,
        ask_human: None,
    }
    .execute_with_literal_context(Some((&error_label, error_msg)))
    .await;
    let (output, reasoning, success) = match outcome {
        Ok((output, reasoning, _usage)) => (output, reasoning, true),
        Err(error) => (error.to_string(), None, false),
    };
    let (output, success, usage) = match handler_usage(&tracker, results.get(error_handler_name)) {
        Ok(usage) => (output, success, usage),
        Err(error) => (error.to_string(), false, UsageSnapshot::unavailable()),
    };
    task_observation.finish(if success {
        crate::metrics::TaskOutcome::Success
    } else {
        crate::metrics::TaskOutcome::Error
    });
    Some(TaskResult {
        task: error_handler_name.clone(),
        agent: error_agent.name.clone(),
        output,
        success,
        duration_ms: error_start.elapsed().as_millis() as u64,
        usage,
        reasoning,
    })
}

fn handler_usage(
    tracker: &crate::usage::UsageTracker,
    previous: Option<&TaskResult>,
) -> crate::utils::error::Result<UsageSnapshot> {
    let mut usage = crate::llm::scope::snapshot(tracker)?;
    if let Some(previous) = previous {
        // Reused handler names retain all disjoint invocations, not just the
        // last output. The enclosing tracker already includes them; do not
        // add the combined result back into that inclusive scope.
        usage
            .settled
            .merge(&previous.usage.settled)
            .map_err(|error| IronCrewError::Provider(error.to_string()))?;
        usage.coverage = usage.settled.coverage();
    }
    Ok(usage)
}
