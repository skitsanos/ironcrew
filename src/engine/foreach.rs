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

const DEFAULT_FOREACH_MAX_ITEMS: usize = 100;
const HARD_FOREACH_MAX_ITEMS: usize = 1_000;
const DEFAULT_FOREACH_MAX_OUTPUT_BYTES: usize = 8 * 1024 * 1024;
const HARD_FOREACH_MAX_OUTPUT_BYTES: usize = 64 * 1024 * 1024;
const DEFAULT_FOREACH_MAX_FIELD_CHARS: usize = 100 * 1024;
const HARD_FOREACH_MAX_FIELD_CHARS: usize = 1024 * 1024;
const HARD_FOREACH_ITEM_VAR_BYTES: usize = 256;
const FOREACH_TRUNCATION_MARKER: &str = "\n[... foreach expansion truncated]";

fn positive_env_usize(name: &str, default: usize, hard_max: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .map(|value| value.min(hard_max))
        .unwrap_or(default)
}

fn encoded_json_string_len(value: &str) -> usize {
    struct Counter(usize);
    impl std::io::Write for Counter {
        fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
            self.0 = self.0.saturating_add(buffer.len());
            Ok(buffer.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    let mut counter = Counter(0);
    if serde_json::to_writer(&mut counter, value).is_err() {
        return usize::MAX;
    }
    counter.0
}

fn reserve_foreach_output(
    task_name: &str,
    output: &str,
    encoded_bytes: &mut usize,
    max_output_bytes: usize,
) -> Result<()> {
    let separator = usize::from(*encoded_bytes > 2);
    let next = encoded_bytes
        .checked_add(separator)
        .and_then(|value| value.checked_add(encoded_json_string_len(output)))
        .ok_or_else(|| crate::utils::error::IronCrewError::Task {
            task: task_name.to_string(),
            message: "foreach output size overflowed".into(),
        })?;
    if next > max_output_bytes {
        return Err(crate::utils::error::IronCrewError::Task {
            task: task_name.to_string(),
            message: format!(
                "foreach output exceeded IRONCREW_FOREACH_MAX_OUTPUT_BYTES ({max_output_bytes})"
            ),
        });
    }
    *encoded_bytes = next;
    Ok(())
}

fn char_prefix(value: &str, max_chars: usize) -> (&str, bool) {
    match value.char_indices().nth(max_chars) {
        Some((boundary, _)) => (&value[..boundary], true),
        None => (value, false),
    }
}

struct BoundedText {
    text: String,
    chars: usize,
    max_chars: usize,
    truncated: bool,
}

impl BoundedText {
    fn new(max_chars: usize) -> Self {
        Self {
            text: String::with_capacity(max_chars.min(16 * 1024)),
            chars: 0,
            max_chars,
            truncated: false,
        }
    }

    fn push(&mut self, value: &str) {
        if self.truncated || value.is_empty() {
            return;
        }

        let remaining = self.max_chars.saturating_sub(self.chars);
        let (prefix, truncated) = char_prefix(value, remaining);
        self.text.push_str(prefix);
        self.chars = self.chars.saturating_add(prefix.chars().count());
        self.truncated = truncated;
    }

    fn mark_truncated(&mut self) {
        self.truncated = true;
    }

    fn finish(mut self) -> (String, bool) {
        let truncated = self.truncated;
        if !truncated {
            return (self.text, false);
        }

        let marker_chars = FOREACH_TRUNCATION_MARKER.chars().count();
        let keep_chars = self.max_chars.saturating_sub(marker_chars);
        if self.chars > keep_chars {
            let (prefix, _) = char_prefix(&self.text, keep_chars);
            self.text.truncate(prefix.len());
            self.chars = keep_chars;
        }

        let remaining = self.max_chars.saturating_sub(self.chars);
        let (marker, _) = char_prefix(FOREACH_TRUNCATION_MARKER, remaining);
        self.text.push_str(marker);
        (self.text, true)
    }
}

struct BoundedJsonWriter {
    bytes: Vec<u8>,
    max_bytes: usize,
    truncated: bool,
}

impl BoundedJsonWriter {
    fn new(max_bytes: usize) -> Self {
        Self {
            bytes: Vec::with_capacity(max_bytes.min(16 * 1024)),
            max_bytes,
            truncated: false,
        }
    }
}

impl std::io::Write for BoundedJsonWriter {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        let remaining = self.max_bytes.saturating_sub(self.bytes.len());
        if buffer.len() <= remaining {
            self.bytes.extend_from_slice(buffer);
            return Ok(buffer.len());
        }

        self.bytes.extend_from_slice(&buffer[..remaining]);
        self.truncated = true;
        Err(std::io::Error::other(
            "foreach item exceeds expansion budget",
        ))
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn bounded_item_text(item: &serde_json::Value, max_chars: usize) -> (String, bool) {
    if let serde_json::Value::String(value) = item {
        let mut text = BoundedText::new(max_chars);
        text.push(value);
        return text.finish();
    }

    // A JSON value can contain a very large string. Stream it into a bounded
    // writer so serializing one foreach item cannot allocate an equally large
    // temporary before the prompt builder gets a chance to truncate it.
    let mut writer = BoundedJsonWriter::new(max_chars);
    let serialization = serde_json::to_writer(&mut writer, item);
    let mut bytes = writer.bytes;
    let valid_bytes = std::str::from_utf8(&bytes)
        .map(|_| bytes.len())
        .unwrap_or_else(|error| error.valid_up_to());
    bytes.truncate(valid_bytes);

    let mut text = BoundedText::new(max_chars);
    // The serializer only emits UTF-8 and the incomplete suffix was removed.
    text.push(std::str::from_utf8(&bytes).unwrap_or_default());
    if writer.truncated || serialization.is_err() {
        text.mark_truncated();
    }
    text.finish()
}

fn replace_bounded(
    template: &str,
    placeholder: &str,
    replacement: &str,
    max_chars: usize,
) -> (String, bool) {
    let mut output = BoundedText::new(max_chars);
    let mut remainder = template;

    while let Some(position) = remainder.find(placeholder) {
        output.push(&remainder[..position]);
        output.push(replacement);
        if output.truncated {
            break;
        }
        remainder = &remainder[position + placeholder.len()..];
    }
    if !output.truncated {
        output.push(remainder);
    }
    output.finish()
}

fn validate_item_var(task: &Task, item_var: &str) -> Result<()> {
    if item_var.is_empty() || item_var.len() > HARD_FOREACH_ITEM_VAR_BYTES {
        return Err(crate::utils::error::IronCrewError::Task {
            task: task.name.clone(),
            message: format!(
                "foreach_as must be between 1 and {HARD_FOREACH_ITEM_VAR_BYTES} UTF-8 bytes"
            ),
        });
    }
    Ok(())
}

/// Build a per-item task from the parent foreach task.
fn build_item_task(
    task: &Task,
    item_var: &str,
    idx: usize,
    total: usize,
    item: &serde_json::Value,
    max_field_chars: usize,
) -> Result<Task> {
    validate_item_var(task, item_var)?;

    let (item_text, item_truncated) = bounded_item_text(item, max_field_chars);
    let mut placeholder = String::with_capacity(item_var.len() + 3);
    placeholder.push_str("${");
    placeholder.push_str(item_var);
    placeholder.push('}');
    let (description, description_truncated) =
        replace_bounded(&task.description, &placeholder, &item_text, max_field_chars);

    let mut context = BoundedText::new(max_field_chars);
    if let Some(existing) = &task.context {
        context.push(existing);
        context.push("\n\n");
    }
    context.push("Processing ");
    context.push(item_var);
    context.push(" ");
    context.push(&(idx + 1).to_string());
    context.push("/");
    context.push(&total.to_string());
    context.push(": ");
    context.push(&item_text);
    let (context, context_truncated) = context.finish();

    if item_truncated || description_truncated || context_truncated {
        tracing::warn!(
            task = %task.name,
            item = idx + 1,
            max_field_chars,
            "foreach item expansion was truncated"
        );
    }

    // Construct explicitly so the original description and context are not
    // cloned before being replaced by their bounded variants.
    Ok(Task {
        name: task.name.clone(),
        description,
        agent: task.agent.clone(),
        expected_output: task.expected_output.clone(),
        context: Some(context),
        depends_on: task.depends_on.clone(),
        max_retries: task.max_retries,
        retry_backoff_secs: task.retry_backoff_secs,
        timeout_secs: task.timeout_secs,
        condition: task.condition.clone(),
        on_error: task.on_error.clone(),
        task_type: task.task_type.clone(),
        collaborative_agents: task.collaborative_agents.clone(),
        max_turns: task.max_turns,
        foreach_source: task.foreach_source.clone(),
        foreach_as: task.foreach_as.clone(),
        foreach_parallel: task.foreach_parallel,
        stream: task.stream,
        model: task.model.clone(),
    })
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
    max_concurrent: usize,
    before_task_hook: Option<&[u8]>,
    after_task_hook: Option<&[u8]>,
    ask_human: Option<&crate::engine::input_bridge::AskHumanContext>,
) -> Result<TaskResult> {
    let item_var = task.foreach_as.as_deref().unwrap_or("item");
    validate_item_var(task, item_var)?;

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
                reasoning: None,
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
            reasoning: None,
        });
    }

    let max_items = positive_env_usize(
        "IRONCREW_FOREACH_MAX_ITEMS",
        DEFAULT_FOREACH_MAX_ITEMS,
        HARD_FOREACH_MAX_ITEMS,
    );
    if items.len() > max_items {
        return Err(crate::utils::error::IronCrewError::Task {
            task: task.name.clone(),
            message: format!(
                "foreach source has {} items, exceeding IRONCREW_FOREACH_MAX_ITEMS ({})",
                items.len(),
                max_items
            ),
        });
    }

    let max_output_bytes = positive_env_usize(
        "IRONCREW_FOREACH_MAX_OUTPUT_BYTES",
        DEFAULT_FOREACH_MAX_OUTPUT_BYTES,
        HARD_FOREACH_MAX_OUTPUT_BYTES,
    );
    let max_field_chars = positive_env_usize(
        "IRONCREW_MAX_PROMPT_CHARS",
        DEFAULT_FOREACH_MAX_FIELD_CHARS,
        HARD_FOREACH_MAX_FIELD_CHARS,
    );

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
    // JSON array delimiters. Each encoded string and comma is counted before
    // retaining the raw output, keeping the final serialized result bounded.
    let mut encoded_output_bytes = 2usize;
    let mut all_success = true;
    let mut accumulated_usage = TaskTokenUsage::default();
    let start = Instant::now();
    let item_count = items.len();

    if task.foreach_parallel {
        // Build item tasks lazily as the buffered stream polls them. Keeping a
        // Task for every item at once would multiply even bounded fields by the
        // full foreach cardinality.
        let item_futures = items.into_iter().enumerate().map(|(idx, item)| async move {
            let item_task =
                build_item_task(task, item_var, idx, item_count, &item, max_field_chars)?;
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
                &item_task,
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
                ask_human,
            )
            .await
        });

        // Bound the fan-out: `buffered` runs at most `max_concurrent` item
        // futures at once (min 1), preserving input order so the index → output
        // mapping below stays correct. Without this cap, a foreach over a large
        // array would fire one LLM request per item simultaneously.
        use futures::stream::StreamExt;
        let parallel_results = futures::stream::iter(item_futures).buffered(max_concurrent.max(1));
        tokio::pin!(parallel_results);

        let mut idx = 0usize;
        while let Some(result) = parallel_results.next().await {
            match result {
                Ok((output, _reasoning, item_usage)) => {
                    if let Some(u) = &item_usage {
                        accumulated_usage.prompt_tokens = accumulated_usage
                            .prompt_tokens
                            .saturating_add(u.prompt_tokens);
                        accumulated_usage.completion_tokens = accumulated_usage
                            .completion_tokens
                            .saturating_add(u.completion_tokens);
                        accumulated_usage.total_tokens = accumulated_usage
                            .total_tokens
                            .saturating_add(u.total_tokens);
                        accumulated_usage.cached_tokens = accumulated_usage
                            .cached_tokens
                            .saturating_add(u.cached_tokens);
                    }
                    reserve_foreach_output(
                        &task.name,
                        &output,
                        &mut encoded_output_bytes,
                        max_output_bytes,
                    )?;
                    foreach_outputs.push(output);
                }
                Err(e) => {
                    tracing::warn!("foreach item {}/{} failed: {}", idx + 1, item_count, e);
                    let output = format!("Error: {}", e);
                    reserve_foreach_output(
                        &task.name,
                        &output,
                        &mut encoded_output_bytes,
                        max_output_bytes,
                    )?;
                    foreach_outputs.push(output);
                    all_success = false;
                }
            }
            idx += 1;
        }
    } else {
        // Sequential: existing behavior
        for (idx, item) in items.iter().enumerate() {
            let item_task =
                build_item_task(task, item_var, idx, items.len(), item, max_field_chars)?;

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
                ask_human,
            )
            .await
            {
                Ok((output, _reasoning, item_usage)) => {
                    if let Some(u) = &item_usage {
                        accumulated_usage.prompt_tokens = accumulated_usage
                            .prompt_tokens
                            .saturating_add(u.prompt_tokens);
                        accumulated_usage.completion_tokens = accumulated_usage
                            .completion_tokens
                            .saturating_add(u.completion_tokens);
                        accumulated_usage.total_tokens = accumulated_usage
                            .total_tokens
                            .saturating_add(u.total_tokens);
                        accumulated_usage.cached_tokens = accumulated_usage
                            .cached_tokens
                            .saturating_add(u.cached_tokens);
                    }
                    reserve_foreach_output(
                        &task.name,
                        &output,
                        &mut encoded_output_bytes,
                        max_output_bytes,
                    )?;
                    foreach_outputs.push(output);
                }
                Err(e) => {
                    tracing::warn!("foreach item {}/{} failed: {}", idx + 1, items.len(), e);
                    let output = format!("Error: {}", e);
                    reserve_foreach_output(
                        &task.name,
                        &output,
                        &mut encoded_output_bytes,
                        max_output_bytes,
                    )?;
                    foreach_outputs.push(output);
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
                "count": item_count,
            }),
        )
        .await?;

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
        reasoning: None,
    })
}

#[cfg(test)]
mod resource_limit_tests {
    use super::*;

    #[test]
    fn encoded_output_budget_counts_json_escaping() {
        let mut bytes = 2;
        reserve_foreach_output("foreach", "\\\"", &mut bytes, 16).unwrap();
        assert_eq!(bytes, serde_json::to_string(&vec!["\\\""]).unwrap().len());

        let cap = bytes + 2;
        let error = reserve_foreach_output("foreach", "too large", &mut bytes, cap).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("IRONCREW_FOREACH_MAX_OUTPUT_BYTES")
        );
    }

    #[test]
    fn repeated_item_expansion_is_bounded_without_splitting_utf8() {
        let template = "${item}".repeat(10_000);
        let replacement = "🦀".repeat(10_000);

        let (expanded, truncated) = replace_bounded(&template, "${item}", &replacement, 128);

        assert!(truncated);
        assert_eq!(expanded.chars().count(), 128);
        assert!(expanded.ends_with(FOREACH_TRUNCATION_MARKER));
        assert!(std::str::from_utf8(expanded.as_bytes()).is_ok());
        assert!(expanded.len() < 1024);
    }

    #[test]
    fn non_string_item_serialization_respects_field_budget() {
        let item = serde_json::json!({"payload": "é".repeat(100_000)});
        let (text, truncated) = bounded_item_text(&item, 96);

        assert!(truncated);
        assert!(text.chars().count() <= 96);
        assert!(text.ends_with(FOREACH_TRUNCATION_MARKER));
        assert!(std::str::from_utf8(text.as_bytes()).is_ok());
    }

    #[test]
    fn item_variable_name_has_a_fixed_hard_limit() {
        let task = Task {
            name: "foreach".into(),
            ..Task::default()
        };
        let oversized = "x".repeat(HARD_FOREACH_ITEM_VAR_BYTES + 1);

        let error = validate_item_var(&task, &oversized).unwrap_err();

        assert!(error.to_string().contains("foreach_as"));
    }
}
