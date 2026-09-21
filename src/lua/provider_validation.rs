//! Provider-aware checks at Lua construction, without invoking the provider.
use crate::engine::{agent::Agent, crew::Crew, task::Task};
use crate::llm::provider::LlmProvider;
use crate::utils::error::Result;

pub(super) fn agent(provider: &dyn LlmProvider, agent: &Agent, crew: &Crew) -> Result<()> {
    let model =
        crate::engine::orchestrator::resolve_model(&Task::default(), agent, crew, "task_execution");
    provider.validate_request(&agent.chat_request(model, vec![]), !agent.tools.is_empty())
}

pub(super) fn crew(provider: &dyn LlmProvider, crew: &Crew) -> Result<()> {
    provider.validate_request(
        &Agent::default().chat_request(crew.provider_config.model.clone(), vec![]),
        false,
    )?;
    for value in &crew.agents {
        agent(provider, value, crew)?;
    }
    Ok(())
}
