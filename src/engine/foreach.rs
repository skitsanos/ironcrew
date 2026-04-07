use std::collections::HashMap;
use std::time::Instant;

use crate::engine::agent::Agent;
use crate::engine::executor::execute_task_standalone_with_hooks;
use crate::engine::memory::MemoryStore;
use crate::engine::messagebus::MessageBus;
use crate::engine::task::{Task, TaskResult, TaskTokenUsage};
use crate::llm::provider::LlmProvider;
use crate::tools::registry::ToolRegistry;
use crate::utils::error::Result;

/// Build a per-item task from the parent foreach task.
fn build_item_task(
    task: &Task,
    item_var: &str,
    idx: usize,
    total: usize,
    item: &serde_json::Value,
) -> Task {
    let item_str = match item {
        serde_json::Value::String(s) => s.clone(),
        other => serde_json::to_string(other).unwrap_or_default(),
    };

    let mut item_task = task.clone();
    let item_context = format!(
        "Processing {} {}/{}: {}",
        item_var,
        idx + 1,
        total,
        item_str
    );
    item_task.context = Some(match &task.context {
        Some(existing) => format!("{}\n\n{}", existing, item_context),
        None => item_context,
    });
    item_task.description = item_task
        .description
        .replace(&format!("${{{}}}", item_var), &item_str);
    item_task
}

#[allow(clippy::too_many_arguments)]
pub async fn execute_foreach_task(
    task: &Task,
    agent: &Agent,
    provider: &dyn LlmProvider,
    tool_registry: &ToolRegistry,
    results: &HashMap<String, TaskResult>,
    memory: &MemoryStore,
    messagebus: &MessageBus,
    model: &str,
    max_tool_rounds: usize,
    stream: bool,
    before_task_hook: Option<&[u8]>,
    after_task_hook: Option<&[u8]>,
) -> Result<TaskResult> {
    let item_var = task
        .foreach_as
        .clone()
        .unwrap_or_else(|| "item".to_string());

    let source_key = task.foreach_source.as_deref().unwrap_or("");

    // Find the source data: check results first, then memory
    let source_data = if let Some(result) = results.get(source_key) {
        // Try to parse the output as a JSON value
        serde_json::from_str::<serde_json::Value>(&result.output).ok()
    } else {
        // Try memory
        memory.get(source_key).await
    };

    let items = match source_data {
        Some(serde_json::Value::Array(arr)) => arr,
        Some(serde_json::Value::String(ref s)) => {
            // Try parsing string as JSON array
            serde_json::from_str::<Vec<serde_json::Value>>(s).unwrap_or_default()
        }
        _ => {
            return Ok(TaskResult {
                task: task.name.clone(),
                agent: String::new(),
                output: format!("Skipped: foreach source '{}' is not an array", source_key),
                success: false,
                duration_ms: 0,
                token_usage: None,
            });
        }
    };

    if items.is_empty() {
        return Ok(TaskResult {
            task: task.name.clone(),
            agent: String::new(),
            output: "Skipped: foreach source is empty".into(),
            success: true,
            duration_ms: 0,
            token_usage: None,
        });
    }

    tracing::info!(
        "Running foreach task '{}' with {} items{}",
        task.name,
        items.len(),
        if task.foreach_parallel {
            " (parallel)"
        } else {
            ""
        }
    );

    let mut foreach_outputs: Vec<String> = Vec::new();
    let mut all_success = true;
    let mut accumulated_usage = TaskTokenUsage::default();
    let start = Instant::now();

    if task.foreach_parallel {
        // Pre-build all item tasks
        let item_tasks: Vec<Task> = items
            .iter()
            .enumerate()
            .map(|(idx, item)| build_item_task(task, &item_var, idx, items.len(), item))
            .collect();

        // Build futures that borrow shared state — join_all runs them concurrently
        // on the current task without spawning, so shared references are valid.
        let futs: Vec<_> = item_tasks
            .iter()
            .map(|item_task| async {
                let mem_ctx = memory.build_context(&item_task.description, 3).await;
                let msgs = messagebus.receive(&agent.name).await;
                let msg_ctx = if msgs.is_empty() {
                    String::new()
                } else {
                    let strs: Vec<String> = msgs
                        .iter()
                        .map(|m| {
                            format!(
                                "[Message from {} ({:?})]: {}",
                                m.from, m.message_type, m.content
                            )
                        })
                        .collect();
                    format!("Messages from other agents:\n{}", strs.join("\n"))
                };
                execute_task_standalone_with_hooks(
                    item_task,
                    agent,
                    provider,
                    tool_registry,
                    results,
                    model,
                    max_tool_rounds,
                    &mem_ctx,
                    &msg_ctx,
                    task.stream || stream,
                    None,
                    None,
                    before_task_hook,
                    after_task_hook,
                )
                .await
            })
            .collect();

        let parallel_results = futures::future::join_all(futs).await;

        // Resize output vec to match items count
        foreach_outputs.resize(items.len(), String::new());

        for (idx, result) in parallel_results.into_iter().enumerate() {
            match result {
                Ok((output, item_usage)) => {
                    if let Some(u) = &item_usage {
                        accumulated_usage.prompt_tokens += u.prompt_tokens;
                        accumulated_usage.completion_tokens += u.completion_tokens;
                        accumulated_usage.total_tokens += u.total_tokens;
                        accumulated_usage.cached_tokens += u.cached_tokens;
                    }
                    foreach_outputs[idx] = output;
                }
                Err(e) => {
                    tracing::warn!("foreach item {}/{} failed: {}", idx + 1, items.len(), e);
                    foreach_outputs[idx] = format!("Error: {}", e);
                    all_success = false;
                }
            }
        }
    } else {
        // Sequential: existing behavior
        for (idx, item) in items.iter().enumerate() {
            let item_task = build_item_task(task, &item_var, idx, items.len(), item);

            let memory_context = memory.build_context(&item_task.description, 3).await;
            let messages_context = messagebus.receive(&agent.name).await;
            let msg_ctx = if messages_context.is_empty() {
                String::new()
            } else {
                let strs: Vec<String> = messages_context
                    .iter()
                    .map(|m| {
                        format!(
                            "[Message from {} ({:?})]: {}",
                            m.from, m.message_type, m.content
                        )
                    })
                    .collect();
                format!("Messages from other agents:\n{}", strs.join("\n"))
            };

            match execute_task_standalone_with_hooks(
                &item_task,
                agent,
                provider,
                tool_registry,
                results,
                model,
                max_tool_rounds,
                &memory_context,
                &msg_ctx,
                task.stream || stream,
                None,
                None,
                before_task_hook,
                after_task_hook,
            )
            .await
            {
                Ok((output, item_usage)) => {
                    if let Some(u) = &item_usage {
                        accumulated_usage.prompt_tokens += u.prompt_tokens;
                        accumulated_usage.completion_tokens += u.completion_tokens;
                        accumulated_usage.total_tokens += u.total_tokens;
                        accumulated_usage.cached_tokens += u.cached_tokens;
                    }
                    foreach_outputs.push(output);
                }
                Err(e) => {
                    tracing::warn!("foreach item {}/{} failed: {}", idx + 1, items.len(), e);
                    foreach_outputs.push(format!("Error: {}", e));
                    all_success = false;
                }
            }
        }
    }

    // Combine all outputs into a JSON array result
    let combined = serde_json::to_string_pretty(&foreach_outputs).unwrap_or_default();

    let duration_ms = start.elapsed().as_millis() as u64;

    if !all_success {
        tracing::warn!("foreach task '{}' had some failures", task.name);
    }

    // Store in memory
    memory
        .set(
            format!("task:{}", task.name),
            serde_json::json!({
                "output": &foreach_outputs,
                "agent": agent.name,
                "count": items.len(),
            }),
        )
        .await;

    let has_usage = accumulated_usage.total_tokens > 0;
    Ok(TaskResult {
        task: task.name.clone(),
        agent: agent.name.clone(),
        output: combined,
        success: all_success,
        duration_ms,
        token_usage: if has_usage {
            Some(accumulated_usage)
        } else {
            None
        },
    })
}
