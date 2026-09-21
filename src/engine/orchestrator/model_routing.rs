use super::*;

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
