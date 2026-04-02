use std::collections::HashMap;
use std::sync::Arc;

use crate::engine::agent::Agent;
use crate::engine::eventbus::{CrewEvent, EventBus};
use crate::engine::task::{TaskResult, TaskTokenUsage};
use crate::llm::provider::*;
use crate::utils::error::{IronCrewError, Result};

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

    let mut total_usage = TaskTokenUsage::default();

    // Build conversation history shared across all agents
    let mut conversation: Vec<String> = Vec::new();
    conversation.push(format!("Task: {}", task_description));

    if !memory_context.is_empty() {
        conversation.push(format!("Context:\n{}", memory_context));
    }

    // Add dependency results as context
    for (name, result) in completed_results {
        if result.success {
            conversation.push(format!("Result from '{}': {}", name, result.output));
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

            // Add the conversation so far
            let conversation_text = conversation.join("\n\n");
            let user_prompt = if turn == 0 && conversation.len() <= 1 {
                format!(
                    "{}\n\nYou are starting the discussion. Share your initial thoughts.",
                    conversation_text
                )
            } else {
                format!(
                    "{}\n\nIt's your turn. Respond to the discussion, adding your perspective.",
                    conversation_text
                )
            };
            messages.push(ChatMessage::user(&user_prompt));

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
            if let Some(usage) = &response.usage {
                total_usage.prompt_tokens += usage.prompt_tokens;
                total_usage.completion_tokens += usage.completion_tokens;
                total_usage.total_tokens += usage.total_tokens;
                total_usage.cached_tokens += usage.cached_tokens;
            }
            let content = response.content.unwrap_or_default();

            conversation.push(format!("[{}]: {}", agent.name, content));

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
    let conversation_text = conversation.join("\n\n");

    let request = ChatRequest {
        messages: vec![
            ChatMessage::system(&system_prompt),
            ChatMessage::user(&format!(
                "Here is the full discussion:\n\n{}\n\nProvide a final synthesized response that combines the best insights from all participants.",
                conversation_text
            )),
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

    let response = provider.chat(request).await?;
    if let Some(usage) = &response.usage {
        total_usage.prompt_tokens += usage.prompt_tokens;
        total_usage.completion_tokens += usage.completion_tokens;
        total_usage.total_tokens += usage.total_tokens;
        total_usage.cached_tokens += usage.cached_tokens;
    }
    let has_usage = total_usage.total_tokens > 0;
    response
        .content
        .map(|c| (c, if has_usage { Some(total_usage) } else { None }))
        .ok_or_else(|| IronCrewError::Provider("Empty synthesis response".into()))
}
