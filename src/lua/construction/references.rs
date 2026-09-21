use mlua::{AnyUserData, Lua};

use crate::lua::crew_userdata::LuaCrew;
use crate::tools::registry::ToolRegistry;
use crate::utils::error::{IronCrewError, Result};

pub(super) async fn registry(lua: &Lua, original: &LuaCrew) -> Result<ToolRegistry> {
    #[cfg(feature = "mcp")]
    if original
        .mcp_config
        .as_ref()
        .is_some_and(|config| !config.is_empty())
    {
        return Err(super::incomplete(lua, "MCP tool discovery"));
    }
    // Finalization may connect MCP servers in run mode. The guard above is
    // mandatory: only local agent-as-tool declarations may be finalized here.
    let _ = lua;
    Ok(original
        .ensure_agent_tools_finalized()
        .await?
        .registry
        .clone())
}

pub(super) async fn validate(lua: &Lua, originals: &[AnyUserData]) -> Result<super::Report> {
    let mut report = super::Report::default();
    for value in originals {
        let original = value.borrow::<LuaCrew>()?;
        let crew = original.crew.lock().await;
        crew.validate_resource_limits()?;
        crate::engine::task::validate_dependency_graph(&crew.tasks)?;
        if !crew.tasks.is_empty() && crew.agents.is_empty() {
            return Err(IronCrewError::Validation(
                "No agents in crew with tasks".into(),
            ));
        }
        let provider = original
            .custom_provider
            .as_ref()
            .unwrap_or(&original.runtime.provider);
        super::super::provider_validation::crew(provider.as_ref(), &crew)?;
        for task in &crew.tasks {
            for name in task.agent.iter().chain(task.collaborative_agents.iter()) {
                if !crew.agents.iter().any(|a| &a.name == name) {
                    return Err(IronCrewError::Validation(format!(
                        "Task '{}' references unknown agent '{name}'",
                        task.name
                    )));
                }
            }
            if let Some(name) = &task.on_error
                && !crew.tasks.iter().any(|t| &t.name == name)
            {
                return Err(IronCrewError::Validation(format!(
                    "Task '{}' references unknown on_error task '{name}'",
                    task.name
                )));
            }
            if let Some(condition) = &task.condition {
                lua.load(format!("return ({condition})")).into_function()?;
            }
            for agent in crew
                .agents
                .iter()
                .filter(|a| task.agent.as_ref().is_none_or(|name| name == &a.name))
            {
                let model = crate::engine::orchestrator::resolve_model(
                    task,
                    agent,
                    &crew,
                    "task_execution",
                );
                provider.validate_request(
                    &agent.chat_request(model, vec![]),
                    !agent.tools.is_empty(),
                )?;
            }
        }
        report.crews += 1;
        report.agents += crew.agents.len();
        report.tasks += crew.tasks.len();
        drop(crew);
        let registry = registry(lua, &original).await?;
        let crew = original.crew.lock().await;
        for agent in &crew.agents {
            for tool in &agent.tools {
                if registry.get(tool).is_none() {
                    return Err(IronCrewError::Validation(format!(
                        "Agent '{}' references unknown tool '{tool}'",
                        agent.name
                    )));
                }
            }
        }
    }
    Ok(report)
}
