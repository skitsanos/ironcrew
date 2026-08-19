use std::collections::HashMap;
use std::sync::Arc;

use crate::engine::agent::Agent;
use crate::engine::eventbus::{CrewEvent, EventBus};
use crate::engine::interpolate::prompt_char_limit;
use crate::engine::task::{TaskResult, TaskTokenUsage};
use crate::llm::provider::*;
use crate::utils::error::{IronCrewError, Result};

mod usage;

use usage::UsageAccumulator;

const DEFAULT_TRANSCRIPT_MAX_BYTES: usize = 8 * 1024 * 1024;
const HARD_TRANSCRIPT_MAX_BYTES: usize = 32 * 1024 * 1024;
const DEFAULT_TURN_MAX_BYTES: usize = 1024 * 1024;
const HARD_TURN_MAX_BYTES: usize = 8 * 1024 * 1024;
const DEFAULT_MAX_PARTICIPANT_TURNS: usize = 64;
const HARD_MAX_PARTICIPANT_TURNS: usize = 512;

fn bounded_env(name: &str, default: usize, hard_max: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .map(|value| value.min(hard_max))
        .unwrap_or(default)
}

struct Transcript {
    entries: Vec<String>,
    bytes: usize,
    max_bytes: usize,
}

impl Transcript {
    fn new(max_bytes: usize) -> Self {
        Self {
            entries: Vec::new(),
            bytes: 0,
            max_bytes,
        }
    }

    fn push(&mut self, task_name: &str, label: &str, value: &str) -> Result<()> {
        let separator = usize::from(!self.entries.is_empty()) * 2;
        let next = self
            .bytes
            .checked_add(separator)
            .and_then(|size| size.checked_add(label.len()))
            .and_then(|size| size.checked_add(value.len()))
            .ok_or_else(|| IronCrewError::Task {
                task: task_name.to_string(),
                message: "collaboration transcript size overflowed".into(),
            })?;
        if next > self.max_bytes {
            return Err(IronCrewError::Task {
                task: task_name.to_string(),
                message: format!(
                    "collaboration transcript exceeds IRONCREW_COLLABORATION_MAX_TRANSCRIPT_BYTES ({})",
                    self.max_bytes
                ),
            });
        }
        let mut entry = String::with_capacity(label.len().saturating_add(value.len()));
        entry.push_str(label);
        entry.push_str(value);
        self.entries.push(entry);
        self.bytes = next;
        Ok(())
    }
}

fn push_chars(output: &mut String, value: &str, chars: &mut usize, max_chars: usize) -> bool {
    if value.is_empty() {
        return false;
    }
    if *chars >= max_chars {
        return true;
    }
    let remaining = max_chars - *chars;
    let mut included = 0usize;
    let mut boundary = value.len();
    let mut truncated = false;
    for (byte_index, _) in value.char_indices() {
        if included == remaining {
            boundary = byte_index;
            truncated = true;
            break;
        }
        included += 1;
    }
    output.push_str(&value[..boundary]);
    *chars += included.min(remaining);
    truncated
}

fn build_bounded_prompt(prefix: &str, transcript: &Transcript, max_chars: usize) -> (String, bool) {
    let mut output = String::with_capacity(max_chars.min(16 * 1024));
    let mut chars = 0usize;
    let mut truncated = push_chars(&mut output, prefix, &mut chars, max_chars);
    for entry in &transcript.entries {
        if truncated {
            break;
        }
        if !output.is_empty() {
            truncated |= push_chars(&mut output, "\n\n", &mut chars, max_chars);
        }
        truncated |= push_chars(&mut output, entry, &mut chars, max_chars);
    }
    (output, truncated)
}

#[allow(clippy::too_many_arguments)]
pub async fn execute_collaborative_task(
    agents: &[&Agent],
    task_name: &str,
    task_description: &str,
    max_turns: usize,
    provider: Arc<dyn LlmProvider>,
    completed_results: &HashMap<String, TaskResult>,
    memory_context: &str,
    model: &str,
    synthesis_model: &str,
    eventbus: &EventBus,
) -> Result<(String, Option<TaskTokenUsage>)> {
    if agents.len() < 2 {
        return Err(IronCrewError::Validation(
            "Collaborative task requires at least 2 agents".into(),
        ));
    }

    let max_participant_turns = bounded_env(
        "IRONCREW_COLLABORATION_MAX_PARTICIPANT_TURNS",
        DEFAULT_MAX_PARTICIPANT_TURNS,
        HARD_MAX_PARTICIPANT_TURNS,
    );
    let requested_turns = agents
        .len()
        .checked_mul(max_turns)
        .ok_or_else(|| IronCrewError::Validation("collaboration turn count overflowed".into()))?;
    if requested_turns > max_participant_turns {
        return Err(IronCrewError::Validation(format!(
            "collaboration requests {requested_turns} participant-turns, exceeding IRONCREW_COLLABORATION_MAX_PARTICIPANT_TURNS ({max_participant_turns})"
        )));
    }

    let transcript_limit = bounded_env(
        "IRONCREW_COLLABORATION_MAX_TRANSCRIPT_BYTES",
        DEFAULT_TRANSCRIPT_MAX_BYTES,
        HARD_TRANSCRIPT_MAX_BYTES,
    );
    let turn_limit = bounded_env(
        "IRONCREW_COLLABORATION_MAX_TURN_BYTES",
        DEFAULT_TURN_MAX_BYTES,
        HARD_TURN_MAX_BYTES,
    )
    .min(transcript_limit);
    let prompt_limit = prompt_char_limit();

    let mut total_usage = UsageAccumulator::default();

    let mut conversation = Transcript::new(transcript_limit);
    conversation.push(task_name, "Task: ", task_description)?;

    if !memory_context.is_empty() {
        conversation.push(task_name, "Context:\n", memory_context)?;
    }

    for (name, result) in completed_results {
        if result.success {
            let label = format!("Result from '{name}': ");
            conversation.push(task_name, &label, &result.output)?;
        }
    }

    for turn in 0..max_turns {
        // Each agent takes a turn
        for agent in agents {
            let system_prompt = agent.system_prompt.clone().unwrap_or_else(|| {
                format!(
                    "You are {} in a collaborative discussion with other agents. Your goal: {}. \
                     Build on what others have said. Be concise and constructive.",
                    agent.name, agent.goal
                )
            });

            let mut messages = vec![ChatMessage::system(&system_prompt)];

            // Build directly into the provider prompt budget. The instruction
            // comes first so transcript truncation cannot remove it.
            let instruction = if turn == 0 && conversation.entries.len() <= 1 {
                "You are starting the discussion. Share your initial thoughts.\n\nDiscussion:\n"
            } else {
                "It's your turn. Respond to the discussion, adding your perspective.\n\nDiscussion:\n"
            };
            let (user_prompt, prompt_truncated) =
                build_bounded_prompt(instruction, &conversation, prompt_limit);
            if prompt_truncated {
                tracing::warn!(
                    task = task_name,
                    max_chars = prompt_limit,
                    "Collaborative provider prompt was truncated"
                );
            }
            messages.push(ChatMessage::user(&user_prompt));
            validate_chat_history(&messages, 1, chat_history_max_bytes(), true)?;

            let agent_model = agent.model.clone().unwrap_or_else(|| model.to_string());

            let request = ChatRequest {
                messages,
                model: agent_model,
                temperature: agent.temperature,
                max_tokens: agent.max_tokens,
                response_format: agent.response_format.clone(),
                prompt_cache_key: None,
                prompt_cache_retention: None,
            };

            let response = provider.chat(request).await?;
            total_usage.observe(response.usage.as_ref());
            let content = response.content.unwrap_or_default();
            if content.len() > turn_limit {
                return Err(IronCrewError::Task {
                    task: task_name.to_string(),
                    message: format!(
                        "collaboration turn from '{}' is {} bytes, exceeding IRONCREW_COLLABORATION_MAX_TURN_BYTES ({turn_limit})",
                        agent.name,
                        content.len()
                    ),
                });
            }

            let label = format!("[{}]: ", agent.name);
            conversation.push(task_name, &label, &content)?;

            eventbus.emit(CrewEvent::CollaborationTurn {
                task: task_name.to_string(),
                agent: agent.name.clone(),
                turn: turn + 1,
                content: content.clone(),
            });

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
    let (synthesis_prompt, synthesis_truncated) = build_bounded_prompt(
        "Provide a final synthesized response that combines the best insights from all participants.\n\nFull discussion:\n",
        &conversation,
        prompt_limit,
    );
    if synthesis_truncated {
        tracing::warn!(
            task = task_name,
            max_chars = prompt_limit,
            "Collaborative synthesis prompt was truncated"
        );
    }

    let request = ChatRequest {
        messages: vec![
            ChatMessage::system(&system_prompt),
            ChatMessage::user(&synthesis_prompt),
        ],
        model: synth_agent
            .model
            .clone()
            .unwrap_or_else(|| synthesis_model.to_string()),
        temperature: synth_agent.temperature,
        max_tokens: synth_agent.max_tokens,
        response_format: synth_agent.response_format.clone(),
        prompt_cache_key: None,
        prompt_cache_retention: None,
    };

    validate_chat_history(&request.messages, 1, chat_history_max_bytes(), true)?;

    let response = provider.chat(request).await?;
    total_usage.observe(response.usage.as_ref());
    response
        .content
        .map(|content| (content, total_usage.finish()))
        .ok_or_else(|| IronCrewError::Provider("Empty synthesis response".into()))
}

#[cfg(test)]
mod tests;
