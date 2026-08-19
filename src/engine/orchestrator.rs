use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::sync::Arc;
use std::time::Instant;

use futures::stream::{FuturesUnordered, StreamExt};

use crate::engine::agent::{Agent, AgentSelector};
use crate::engine::collaborative::execute_collaborative_task;
use crate::engine::condition::evaluate_condition;
use crate::engine::crew::Crew;
use crate::engine::eventbus::{CrewEvent, TokenUsageSummary};
use crate::engine::executor::execute_task_standalone;
use crate::engine::foreach::execute_foreach_task;
use crate::engine::interpolate::interpolate;
use crate::engine::task::{
    Task, TaskResult, TaskTokenUsage, topological_phases, validate_dependency_graph,
};
use crate::engine::task_runner::{handle_task_error, run_single_task};
use crate::llm::provider::LlmProvider;
use crate::tools::registry::ToolRegistry;
use crate::utils::error::{IronCrewError, Result};

const DEFAULT_MAX_CONCURRENT_TASKS: usize = 32;
const HARD_MAX_CONCURRENT_TASKS: usize = 256;
const DEFAULT_TASK_RESULT_MAX_OUTPUT_BYTES: usize = 8 * 1024 * 1024;
const HARD_TASK_RESULT_MAX_OUTPUT_BYTES: usize = 32 * 1024 * 1024;
const DEFAULT_TASK_RESULT_MAX_REASONING_BYTES: usize = 4 * 1024 * 1024;
const HARD_TASK_RESULT_MAX_REASONING_BYTES: usize = 16 * 1024 * 1024;
const DEFAULT_RUN_RESULTS_MAX_BYTES: usize = 32 * 1024 * 1024;
// The JSON store defaults to 64 MiB per record. Keep at least 16 MiB for the
// RunRecord envelope, tags, goal, and JSON escaping/metadata overhead.
const HARD_RUN_RESULTS_MAX_BYTES: usize = 48 * 1024 * 1024;

fn configured_byte_limit(name: &str, default: usize, hard_max: usize) -> Result<usize> {
    match std::env::var(name) {
        Ok(raw) => {
            let value = raw.parse::<usize>().map_err(|_| {
                IronCrewError::Validation(format!(
                    "{name} must be an integer between 1 and {hard_max}"
                ))
            })?;
            if value == 0 || value > hard_max {
                return Err(IronCrewError::Validation(format!(
                    "{name} must be between 1 and {hard_max}; got {value}"
                )));
            }
            Ok(value)
        }
        Err(std::env::VarError::NotPresent) => Ok(default),
        Err(std::env::VarError::NotUnicode(_)) => Err(IronCrewError::Validation(format!(
            "{name} must contain valid UTF-8"
        ))),
    }
}

#[derive(Debug)]
struct CountingWriter {
    bytes: usize,
}

impl Write for CountingWriter {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        self.bytes = self
            .bytes
            .checked_add(buffer.len())
            .ok_or_else(|| std::io::Error::other("serialized TaskResult size overflow"))?;
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn serialized_result_bytes(result: &TaskResult) -> Result<usize> {
    let mut writer = CountingWriter { bytes: 0 };
    serde_json::to_writer(&mut writer, result).map_err(|error| {
        IronCrewError::Validation(format!(
            "Failed to size task result '{}': {error}",
            result.task
        ))
    })?;
    Ok(writer.bytes)
}

/// Tracks the serialized bytes retained in the run result map. This is the
/// representation ultimately persisted, so the aggregate ceiling protects
/// both process RSS and the 64 MiB JSON-store record budget.
struct RetainedResultBudget {
    max_output_bytes: usize,
    max_reasoning_bytes: usize,
    max_total_bytes: usize,
    total_bytes: usize,
}

impl RetainedResultBudget {
    fn from_env() -> Result<Self> {
        Ok(Self {
            max_output_bytes: configured_byte_limit(
                "IRONCREW_TASK_RESULT_MAX_OUTPUT_BYTES",
                DEFAULT_TASK_RESULT_MAX_OUTPUT_BYTES,
                HARD_TASK_RESULT_MAX_OUTPUT_BYTES,
            )?,
            max_reasoning_bytes: configured_byte_limit(
                "IRONCREW_TASK_RESULT_MAX_REASONING_BYTES",
                DEFAULT_TASK_RESULT_MAX_REASONING_BYTES,
                HARD_TASK_RESULT_MAX_REASONING_BYTES,
            )?,
            max_total_bytes: configured_byte_limit(
                "IRONCREW_RUN_RESULTS_MAX_BYTES",
                DEFAULT_RUN_RESULTS_MAX_BYTES,
                HARD_RUN_RESULTS_MAX_BYTES,
            )?,
            total_bytes: 0,
        })
    }

    fn insert(
        &mut self,
        results: &mut HashMap<String, TaskResult>,
        key: String,
        result: TaskResult,
    ) -> Result<()> {
        if result.output.len() > self.max_output_bytes {
            return Err(IronCrewError::Validation(format!(
                "Task '{}' output is {} bytes, exceeds IRONCREW_TASK_RESULT_MAX_OUTPUT_BYTES ({})",
                result.task,
                result.output.len(),
                self.max_output_bytes
            )));
        }
        if let Some(reasoning) = result.reasoning.as_ref()
            && reasoning.len() > self.max_reasoning_bytes
        {
            return Err(IronCrewError::Validation(format!(
                "Task '{}' reasoning is {} bytes, exceeds IRONCREW_TASK_RESULT_MAX_REASONING_BYTES ({})",
                result.task,
                reasoning.len(),
                self.max_reasoning_bytes
            )));
        }

        let result_bytes = serialized_result_bytes(&result)?;
        let replaced_bytes = results
            .get(&key)
            .map(serialized_result_bytes)
            .transpose()?
            .unwrap_or(0);
        let new_total = self
            .total_bytes
            .saturating_sub(replaced_bytes)
            .checked_add(result_bytes)
            .ok_or_else(|| IronCrewError::Validation("Run result byte count overflowed".into()))?;
        if new_total > self.max_total_bytes {
            return Err(IronCrewError::Validation(format!(
                "Retaining task '{}' would grow serialized run results to {} bytes, exceeding IRONCREW_RUN_RESULTS_MAX_BYTES ({})",
                result.task, new_total, self.max_total_bytes
            )));
        }

        results.insert(key, result);
        self.total_bytes = new_total;
        Ok(())
    }
}

/// Resolve the model to use for a task, following the priority chain:
/// 1. Agent's model override
/// 2. Task's model override
/// 3. Model Router purpose-based mapping
/// 4. Crew's default model
pub fn resolve_model(task: &Task, agent: &Agent, crew: &Crew, purpose: &str) -> String {
    // 1. Agent's model override
    if let Some(ref model) = agent.model {
        return model.clone();
    }
    // 2. Task's model override
    if let Some(ref model) = task.model {
        return model.clone();
    }
    // 3. Model Router purpose-based
    if crew.model_router.is_configured() {
        return crew
            .model_router
            .resolve(purpose, &crew.provider_config.model);
    }
    // 4. Crew default
    crew.provider_config.model.clone()
}

/// Filter tasks in a phase to only those eligible for execution.
/// Skips error handlers, tasks with failed dependencies, and tasks whose conditions are false.
fn filter_eligible_tasks<'a>(
    phase: &[&'a Task],
    error_handler_names: &HashSet<&str>,
    failed_tasks: &mut HashSet<String>,
    results: &mut HashMap<String, TaskResult>,
    result_budget: &mut RetainedResultBudget,
    crew: &Crew,
) -> Result<Vec<&'a Task>> {
    let mut eligible = Vec::new();

    for task in phase {
        // Skip error handler tasks -- they run only when triggered
        if error_handler_names.contains(task.name.as_str()) {
            continue;
        }

        // Check if any dependency failed
        if let Some(failed_dep) = task.depends_on.iter().find(|d| failed_tasks.contains(*d)) {
            crate::engine::task_observation::record_skipped();
            let reason = format!("dependency '{}' failed", failed_dep);
            crew.eventbus.emit(CrewEvent::TaskSkipped {
                task: task.name.clone(),
                reason: reason.clone(),
            });
            let result = TaskResult {
                task: task.name.clone(),
                agent: String::new(),
                output: format!("Skipped: {}", reason),
                success: false,
                duration_ms: 0,
                token_usage: None,
                reasoning: None,
            };
            failed_tasks.insert(task.name.clone());
            result_budget.insert(results, task.name.clone(), result)?;
            tracing::warn!(
                "Skipping task '{}': dependency '{}' failed",
                task.name,
                failed_dep
            );
            continue;
        }

        // Check condition if present
        if let Some(ref condition) = task.condition {
            let interpolated_condition = interpolate(condition, results);
            let should_run = evaluate_condition(&interpolated_condition, results);
            if !should_run {
                crate::engine::task_observation::record_skipped();
                crew.eventbus.emit(CrewEvent::TaskSkipped {
                    task: task.name.clone(),
                    reason: format!("condition '{}' evaluated to false", condition),
                });
                let result = TaskResult {
                    task: task.name.clone(),
                    agent: String::new(),
                    output: format!("Skipped: condition '{}' evaluated to false", condition),
                    success: true,
                    duration_ms: 0,
                    token_usage: None,
                    reasoning: None,
                };
                result_budget.insert(results, task.name.clone(), result)?;
                tracing::info!(
                    "Skipping task '{}': condition '{}' is false",
                    task.name,
                    condition
                );
                continue;
            }
        }

        eligible.push(*task);
    }

    Ok(eligible)
}

/// The result type from each concurrent task future.
/// Fields: task_name, agent_name, output_result, duration_ms, token_usage, reasoning
type TaskFutureResult = (
    String,
    String,
    Result<String>,
    u64,
    Option<TaskTokenUsage>,
    Option<String>,
);

/// Process one completed concurrent task future immediately. Avoid retaining a
/// second phase-sized vector of outputs while all results are already complete.
async fn process_phase_result(
    phase_result: TaskFutureResult,
    crew: &Crew,
    provider: &Arc<dyn LlmProvider>,
    tool_registry: &ToolRegistry,
    results: &mut HashMap<String, TaskResult>,
    result_budget: &mut RetainedResultBudget,
    failed_tasks: &mut HashSet<String>,
) -> Result<()> {
    let (task_name, agent_name, output, duration_ms, token_usage, reasoning) = phase_result;
    match output {
        Ok(out) => {
            let result = TaskResult {
                task: task_name.clone(),
                agent: agent_name.clone(),
                output: out,
                success: true,
                duration_ms,
                token_usage,
                reasoning,
            };
            result_budget.insert(results, task_name.clone(), result)?;
            let retained = results
                .get(&task_name)
                .expect("result was inserted immediately above");
            if let Some(ref reasoning) = retained.reasoning {
                crew.eventbus.emit(CrewEvent::TaskThinking {
                    task: task_name.clone(),
                    agent: agent_name.clone(),
                    content: reasoning.clone(),
                });
            }
            crew.eventbus.emit(CrewEvent::TaskCompleted {
                task: task_name.clone(),
                agent: agent_name,
                duration_ms,
                success: true,
                output: retained.output.clone(),
                token_usage: retained.token_usage.as_ref().map(|u| TokenUsageSummary {
                    prompt_tokens: u.prompt_tokens,
                    completion_tokens: u.completion_tokens,
                    total_tokens: u.total_tokens,
                    cached_tokens: u.cached_tokens,
                }),
            });
            tracing::info!("Task '{}' completed in {}ms", task_name, duration_ms);
        }
        Err(e) => {
            let error_msg = e.to_string();

            // Check if this task has an on_error handler
            let task_def = crew.tasks.iter().find(|t| t.name == task_name);
            if let Some(task_def) = task_def
                && task_def.on_error.is_some()
                && let Some((mut recovered, handler_result)) = handle_task_error(
                    task_def,
                    &agent_name,
                    &error_msg,
                    &crew.tasks,
                    &crew.agents,
                    provider.clone(),
                    tool_registry,
                    results,
                    &crew.memory,
                    &crew.provider_config.model,
                    crew.max_tool_rounds,
                )
                .await
            {
                recovered.duration_ms = duration_ms;
                result_budget.insert(results, task_name, recovered)?;
                if let Some(hr) = handler_result {
                    result_budget.insert(results, hr.task.clone(), hr)?;
                }
                return Ok(());
            }

            // Original failure path (no handler or handler failed)
            crew.eventbus.emit(CrewEvent::TaskFailed {
                task: task_name.clone(),
                agent: agent_name.clone(),
                error: error_msg.clone(),
                duration_ms,
            });
            let result = TaskResult {
                task: task_name.clone(),
                agent: agent_name,
                output: error_msg,
                success: false,
                duration_ms,
                token_usage: None,
                reasoning: None,
            };
            tracing::error!("Task '{}' failed: {}", task_name, e);
            failed_tasks.insert(task_name.clone());
            result_budget.insert(results, task_name, result)?;
        }
    }

    Ok(())
}

pub async fn run_crew(
    crew: &Crew,
    provider: Arc<dyn LlmProvider>,
    tool_registry: &ToolRegistry,
) -> Result<Vec<TaskResult>> {
    crew.validate_resource_limits()?;
    if crew.agents.is_empty() {
        return Err(IronCrewError::Validation("No agents in crew".into()));
    }
    if crew.tasks.is_empty() {
        return Err(IronCrewError::Validation("No tasks in crew".into()));
    }

    crew.eventbus.emit(CrewEvent::CrewStarted {
        goal: crew.goal.clone(),
        agent_count: crew.agents.len(),
        task_count: crew.tasks.len(),
        model: crew.provider_config.model.clone(),
    });

    // Register all agents in the messagebus
    for agent in &crew.agents {
        crew.messagebus.register_agent(&agent.name).await;
    }
    // Clear pending broadcasts now that all agents have received them
    crew.messagebus.clear_pending_broadcasts().await;

    validate_dependency_graph(&crew.tasks)?;
    let phases = topological_phases(&crew.tasks);

    let mut results: HashMap<String, TaskResult> = HashMap::new();
    let mut result_budget = RetainedResultBudget::from_env()?;
    let mut failed_tasks: HashSet<String> = HashSet::new();
    // Track task names already persisted to memory, so we only write each once
    // across phases (previously the loop re-wrote every successful result every phase).
    let mut persisted_to_memory: HashSet<String> = HashSet::new();

    // Collect error handler task names so we can skip them in normal execution
    let error_handler_names: HashSet<&str> = crew
        .tasks
        .iter()
        .filter_map(|t| t.on_error.as_deref())
        .collect();

    // Concurrency limit: crew config > env var > conservative default (4)
    let max_concurrent = crew.max_concurrent_tasks.unwrap_or_else(|| {
        std::env::var("IRONCREW_DEFAULT_MAX_CONCURRENT")
            .ok()
            .and_then(|v| v.parse().ok())
            .filter(|value| *value > 0 && *value <= HARD_MAX_CONCURRENT_TASKS)
            .unwrap_or(4)
    });
    let hard_concurrency_limit = std::env::var("IRONCREW_MAX_CONCURRENT_TASKS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0 && *value <= HARD_MAX_CONCURRENT_TASKS)
        .unwrap_or(DEFAULT_MAX_CONCURRENT_TASKS);
    if max_concurrent == 0 || max_concurrent > hard_concurrency_limit {
        return Err(IronCrewError::Validation(format!(
            "Crew max_concurrent must be between 1 and IRONCREW_MAX_CONCURRENT_TASKS ({hard_concurrency_limit})"
        )));
    }
    let semaphore = Some(Arc::new(tokio::sync::Semaphore::new(max_concurrent)));

    // Build a flat ordering of task names for final result ordering
    let task_order: Vec<&str> = phases
        .iter()
        .flat_map(|phase| phase.iter().map(|t| t.name.as_str()))
        .collect();

    for (phase_idx, phase) in phases.iter().enumerate() {
        let phase_tasks = filter_eligible_tasks(
            phase,
            &error_handler_names,
            &mut failed_tasks,
            &mut results,
            &mut result_budget,
            crew,
        )?;

        if phase_tasks.is_empty() {
            continue;
        }

        crew.eventbus.emit(CrewEvent::PhaseStart {
            phase: phase_idx,
            tasks: phase_tasks.iter().map(|t| t.name.clone()).collect(),
        });

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
        let mut standard_tasks: Vec<&Task> = Vec::new();
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

                crew.eventbus.emit(CrewEvent::TaskAssigned {
                    task: task.name.clone(),
                    agent: agent.name.clone(),
                    phase: phase_idx,
                });

                let model = resolve_model(task, agent, crew, "task_execution");

                let before_hook = crew
                    .before_task_hooks
                    .get(&agent.name)
                    .map(|v| v.as_slice());
                let after_hook = crew.after_task_hooks.get(&agent.name).map(|v| v.as_slice());

                let task_observation = crate::engine::task_observation::TaskObservation::start();
                let foreach_outcome = match execute_foreach_task(
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
                    max_concurrent,
                    before_hook,
                    after_hook,
                    crew.ask_human.as_ref(),
                )
                .await
                {
                    Ok(result) => result,
                    Err(error) => {
                        task_observation.finish(crate::metrics::TaskOutcome::Error);
                        return Err(error);
                    }
                };

                let foreach_wholly_failed = foreach_outcome.is_wholly_failed();
                let foreach_result = foreach_outcome.result;

                // The foreach executor leaves the agent empty only for its two
                // explicit skip paths (missing/non-array input and empty input).
                // Do not derive metric semantics from human-readable output.
                let foreach_skipped = foreach_result.agent.is_empty();
                if foreach_skipped {
                    task_observation.finish_skipped();
                } else {
                    task_observation.finish(if foreach_result.success {
                        crate::metrics::TaskOutcome::Success
                    } else {
                        crate::metrics::TaskOutcome::Error
                    });
                }

                // A foreach fails the task when its source was unusable, or
                // when every item it ran errored — in both cases dependents
                // would execute with no usable input, so gate them the same
                // way a failed standard task does.
                if !foreach_result.success && (foreach_skipped || foreach_wholly_failed) {
                    crew.eventbus.emit(CrewEvent::TaskFailed {
                        task: task.name.clone(),
                        agent: agent.name.clone(),
                        error: foreach_result.output.clone(),
                        duration_ms: foreach_result.duration_ms,
                    });
                    if foreach_skipped {
                        tracing::warn!(
                            "foreach source for task '{}' is not an array, skipping",
                            task.name
                        );
                    } else {
                        tracing::warn!("every item of foreach task '{}' failed", task.name);
                    }
                    failed_tasks.insert(task.name.clone());
                } else {
                    crew.eventbus.emit(CrewEvent::TaskCompleted {
                        task: task.name.clone(),
                        agent: agent.name.clone(),
                        duration_ms: foreach_result.duration_ms,
                        success: foreach_result.success,
                        output: foreach_result.output.clone(),
                        token_usage: foreach_result.token_usage.as_ref().map(|u| {
                            TokenUsageSummary {
                                prompt_tokens: u.prompt_tokens,
                                completion_tokens: u.completion_tokens,
                                total_tokens: u.total_tokens,
                                cached_tokens: u.cached_tokens,
                            }
                        }),
                    });
                }

                result_budget.insert(&mut results, task.name.clone(), foreach_result)?;
                continue; // Don't go through normal spawn path
            } else if task.task_type.as_deref() == Some("collaborative")
                && task.collaborative_agents.len() >= 2
            {
                crew.eventbus.emit(CrewEvent::TaskAssigned {
                    task: task.name.clone(),
                    agent: task.collaborative_agents.join("+"),
                    phase: phase_idx,
                });

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

                // Resolve collaboration model: use task model override if set,
                // otherwise use router or crew default
                let collab_model = if let Some(ref m) = task.model {
                    m.clone()
                } else if crew.model_router.is_configured() {
                    crew.model_router
                        .resolve("collaboration", &crew.provider_config.model)
                } else {
                    crew.provider_config.model.clone()
                };

                let collab_synthesis_model = if let Some(ref m) = task.model {
                    m.clone()
                } else if crew.model_router.is_configured() {
                    crew.model_router
                        .resolve("collaboration_synthesis", &crew.provider_config.model)
                } else {
                    crew.provider_config.model.clone()
                };

                let start = Instant::now();
                let task_observation = crate::engine::task_observation::TaskObservation::start();
                match execute_collaborative_task(
                    &collab_agents,
                    &task.name,
                    &interpolate(&task.description, &results),
                    max_turns,
                    provider.clone(),
                    &results,
                    &memory_context,
                    &collab_model,
                    &collab_synthesis_model,
                    &crew.eventbus,
                )
                .await
                {
                    Ok((output, collab_usage)) => {
                        task_observation.finish(crate::metrics::TaskOutcome::Success);
                        let duration_ms = start.elapsed().as_millis() as u64;
                        crew.eventbus.emit(CrewEvent::TaskCompleted {
                            task: task.name.clone(),
                            agent: task.collaborative_agents.join("+"),
                            duration_ms,
                            success: true,
                            output: output.clone(),
                            token_usage: collab_usage.as_ref().map(|u| TokenUsageSummary {
                                prompt_tokens: u.prompt_tokens,
                                completion_tokens: u.completion_tokens,
                                total_tokens: u.total_tokens,
                                cached_tokens: u.cached_tokens,
                            }),
                        });
                        tracing::info!(
                            "Collaborative task '{}' completed in {}ms",
                            task.name,
                            duration_ms
                        );
                        result_budget.insert(
                            &mut results,
                            task.name.clone(),
                            TaskResult {
                                task: task.name.clone(),
                                agent: task.collaborative_agents.join("+"),
                                output,
                                success: true,
                                duration_ms,
                                token_usage: collab_usage,
                                reasoning: None,
                            },
                        )?;
                    }
                    Err(e) => {
                        task_observation.finish(crate::metrics::TaskOutcome::Error);
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
                                    error_task
                                        .context
                                        .as_ref()
                                        .map_or(error_context.clone(), |existing| {
                                            format!("{}\n\n{}", existing, error_context)
                                        }),
                                );

                                let error_agent = if let Some(ref ea_name) = error_task.agent {
                                    crew.agents
                                        .iter()
                                        .find(|a| a.name == *ea_name)
                                        .unwrap_or(&crew.agents[0])
                                } else {
                                    AgentSelector::select(&crew.agents, &error_task)
                                };

                                let error_model =
                                    resolve_model(&error_task, error_agent, crew, "task_execution");
                                let error_start = Instant::now();
                                let handler_observation =
                                    crate::engine::task_observation::TaskObservation::start();
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
                                    Ok((output, handler_reasoning, handler_usage)) => {
                                        handler_observation
                                            .finish(crate::metrics::TaskOutcome::Success);
                                        result_budget.insert(
                                            &mut results,
                                            task.name.clone(),
                                            TaskResult {
                                                task: task.name.clone(),
                                                agent: task.collaborative_agents.join("+"),
                                                output: format!(
                                                    "Recovered via '{}': {}",
                                                    error_handler_name, output
                                                ),
                                                success: true,
                                                duration_ms,
                                                token_usage: None,
                                                reasoning: None,
                                            },
                                        )?;
                                        result_budget.insert(
                                            &mut results,
                                            error_handler_name.clone(),
                                            TaskResult {
                                                task: error_handler_name.clone(),
                                                agent: error_agent.name.clone(),
                                                output,
                                                success: true,
                                                duration_ms: error_start.elapsed().as_millis()
                                                    as u64,
                                                token_usage: handler_usage,
                                                reasoning: handler_reasoning,
                                            },
                                        )?;
                                        continue;
                                    }
                                    Err(handler_err) => {
                                        handler_observation
                                            .finish(crate::metrics::TaskOutcome::Error);
                                        tracing::error!(
                                            "Error handler '{}' also failed: {}",
                                            error_handler_name,
                                            handler_err
                                        );
                                    }
                                }
                            }
                        }

                        crew.eventbus.emit(CrewEvent::TaskFailed {
                            task: task.name.clone(),
                            agent: task.collaborative_agents.join("+"),
                            error: error_msg.clone(),
                            duration_ms,
                        });
                        tracing::error!("Collaborative task '{}' failed: {}", task.name, e);
                        failed_tasks.insert(task.name.clone());
                        result_budget.insert(
                            &mut results,
                            task.name.clone(),
                            TaskResult {
                                task: task.name.clone(),
                                agent: task.collaborative_agents.join("+"),
                                output: error_msg,
                                success: false,
                                duration_ms,
                                token_usage: None,
                                reasoning: None,
                            },
                        )?;
                    }
                }
            } else {
                standard_tasks.push(task);
            }
        }

        // Run all standard tasks in this phase concurrently using FuturesUnordered.
        // Unlike tokio::spawn, these futures run on the current task — when the
        // orchestrator is aborted (e.g., API timeout), all in-flight futures are
        // dropped and their resources (HTTP connections, memory) freed immediately.
        let mut futures = FuturesUnordered::new();

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

            crew.eventbus.emit(CrewEvent::TaskAssigned {
                task: task.name.clone(),
                agent: agent.name.clone(),
                phase: phase_idx,
            });
            tracing::info!("Task '{}' assigned to agent '{}'", task.name, agent.name);

            let task_owned = (*task).clone();
            let agent_owned = agent.clone();
            let provider_clone = provider.clone();
            let tool_registry_clone = tool_registry.clone();
            // Share only the results this task depends on (not the full map).
            // TaskResult is cloned here but within a phase, tasks only depend
            // on results from prior phases, so this is typically 0-3 entries.
            let results_snapshot: HashMap<String, TaskResult> = task
                .depends_on
                .iter()
                .filter_map(|dep| results.get(dep).map(|r| (dep.clone(), r.clone())))
                .collect();
            let model = resolve_model(task, agent, crew, "task_execution");
            let max_tool_rounds = crew.max_tool_rounds;
            let should_stream = task.stream || crew.stream;
            let sem = semaphore.clone();
            let memory = crew.memory.clone();
            let messagebus = crew.messagebus.clone();
            let before_hook = crew.before_task_hooks.get(&agent.name).cloned();
            let after_hook = crew.after_task_hooks.get(&agent.name).cloned();
            let ask_human = crew.ask_human.clone();

            futures.push(async move {
                let _permit = match sem {
                    Some(ref s) => Some(s.acquire().await.unwrap()),
                    None => None,
                };

                run_single_task(
                    &task_owned,
                    &agent_owned,
                    provider_clone,
                    tool_registry_clone,
                    results_snapshot,
                    model,
                    max_tool_rounds,
                    &memory,
                    &messagebus,
                    should_stream,
                    before_hook,
                    after_hook,
                    ask_human,
                )
                .await
            });
        }

        // Process each completion immediately so a phase does not retain all
        // completed outputs in both FuturesUnordered and a second Vec.
        while let Some(result) = futures.next().await {
            process_phase_result(
                result,
                crew,
                &provider,
                tool_registry,
                &mut results,
                &mut result_budget,
                &mut failed_tasks,
            )
            .await?;
        }

        // Store successful task results in memory. Only persist each task
        // once across all phases — previously this loop iterated the whole
        // results map every phase and re-wrote already-stored entries.
        for (task_name, result) in &results {
            if result.success && persisted_to_memory.insert(task_name.clone()) {
                let value = serde_json::json!({
                    "output": result.output,
                    "agent": result.agent,
                    "duration_ms": result.duration_ms,
                });
                // Best-effort: a task that already produced output within the
                // task-result cap must not fail the run because the value
                // exceeds the (smaller) memory value cap. Report instead.
                if let Err(e) = crew.memory.set(format!("task:{}", task_name), value).await {
                    tracing::warn!(
                        "Task '{}' result was not stored in memory: {}",
                        task_name,
                        e
                    );
                    crew.eventbus.emit(CrewEvent::Log {
                        level: "warn".into(),
                        message: format!("Task '{task_name}' result not stored in memory: {e}"),
                    });
                }
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
            crate::engine::task_observation::record_skipped();
            result_budget.insert(
                &mut results,
                handler_name.clone(),
                TaskResult {
                    task: handler_name.clone(),
                    agent: String::new(),
                    output: "Skipped: error handler not triggered".into(),
                    success: true,
                    duration_ms: 0,
                    token_usage: None,
                    reasoning: None,
                },
            )?;
        }
    }

    // Persist memory if using persistent backend. Log and emit a warning
    // event on failure — operators should know if durable state is lost.
    if let Err(e) = crew.memory.save().await {
        tracing::error!("Failed to persist memory at end of run: {}", e);
        crew.eventbus.emit(CrewEvent::Log {
            level: "error".into(),
            message: format!("Memory persistence failed: {}", e),
        });
    }

    // Note: RunComplete is NOT emitted here — the API handler is responsible
    // for emitting it with the correct run_id after the Lua script fully completes.

    // Return results in phase order
    Ok(task_order
        .iter()
        .filter_map(|name| results.remove(*name))
        .collect())
}

#[cfg(test)]
mod tests;
