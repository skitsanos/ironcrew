use std::collections::HashMap;
use std::time::Instant;

use crate::engine::agent::Agent;
use crate::engine::executor::execute_task_standalone;
use crate::engine::memory::MemoryStore;
use crate::engine::messagebus::MessageBus;
use crate::engine::task::{Task, TaskResult};
use crate::llm::provider::LlmProvider;
use crate::tools::registry::ToolRegistry;
use crate::utils::error::Result;

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
                output: format!(
                    "Skipped: foreach source '{}' is not an array",
                    source_key
                ),
                success: false,
                duration_ms: 0,
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
        });
    }

    tracing::info!(
        "Running foreach task '{}' with {} items",
        task.name,
        items.len()
    );

    // Run each item sequentially, collecting individual results
    let mut foreach_outputs: Vec<String> = Vec::new();
    let mut all_success = true;
    let start = Instant::now();

    for (idx, item) in items.iter().enumerate() {
        let item_str = match item {
            serde_json::Value::String(s) => s.clone(),
            other => serde_json::to_string(other).unwrap_or_default(),
        };

        // Create a modified task with the item injected into context
        let mut item_task = task.clone();
        let item_context = format!(
            "Processing {} {}/{}: {}",
            item_var,
            idx + 1,
            items.len(),
            item_str
        );
        item_task.context = Some(match &task.context {
            Some(existing) => format!("{}\n\n{}", existing, item_context),
            None => item_context,
        });
        // Interpolate the description with the item
        item_task.description = item_task
            .description
            .replace(&format!("${{{}}}", item_var), &item_str);

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

        match execute_task_standalone(
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
        )
        .await
        {
            Ok(output) => {
                foreach_outputs.push(output);
            }
            Err(e) => {
                tracing::warn!(
                    "foreach item {}/{} failed: {}",
                    idx + 1,
                    items.len(),
                    e
                );
                foreach_outputs.push(format!("Error: {}", e));
                all_success = false;
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

    Ok(TaskResult {
        task: task.name.clone(),
        agent: agent.name.clone(),
        output: combined,
        success: all_success,
        duration_ms,
    })
}
