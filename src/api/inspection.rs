//! Blocking flow-inspection work executed behind HTTP admission.

use axum::http::StatusCode;

use super::handlers::{flow_status, sanitize_error};
use super::{AppState, resolve_flow_path};

pub(super) type InspectionFailure = (StatusCode, String);

fn load_project(
    state: &AppState,
    flow: &str,
) -> Result<(std::path::PathBuf, crate::lua::loader::ProjectLoader), InspectionFailure> {
    use crate::lua::loader::ProjectLoader;

    let flow_path = resolve_flow_path(state, flow)
        .map_err(|error| (flow_status(&error), sanitize_error(&error)))?;
    let loader = if flow_path.is_file() {
        ProjectLoader::from_file(&flow_path)
    } else {
        ProjectLoader::from_directory(&flow_path)
    }
    .map_err(|error| (StatusCode::BAD_REQUEST, sanitize_error(&error)))?;
    Ok((flow_path, loader))
}

pub(super) fn validate_flow(
    state: &AppState,
    flow: String,
) -> Result<serde_json::Value, InspectionFailure> {
    use crate::lua::api::{load_agents_from_files, load_tool_defs_from_files};
    use crate::lua::sandbox::create_tool_lua;

    let (flow_path, loader) = load_project(state, &flow)?;
    let lua = create_tool_lua().map_err(|error| {
        tracing::error!(%error, "tool Lua VM could not be created for flow validation");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Internal server error".to_string(),
        )
    })?;
    let agents = load_agents_from_files(loader.agent_files())
        .map_err(|error| (StatusCode::BAD_REQUEST, sanitize_error(&error)))?;
    let tool_defs = load_tool_defs_from_files(loader.tool_files())
        .map_err(|error| (StatusCode::BAD_REQUEST, sanitize_error(&error)))?;
    let entrypoint_valid = loader.entrypoint().is_some_and(|entrypoint| {
        crate::lua::source::read_lua_source(entrypoint)
            .ok()
            .is_some_and(|script| lua.load(&script).into_function().is_ok())
    });
    Ok(serde_json::json!({
        "flow": flow,
        "valid": entrypoint_valid,
        "agents": agents.iter().map(|agent| serde_json::json!({
            "name": agent.name,
            "goal": agent.goal,
            "capabilities": agent.capabilities,
            "tools": agent.tools,
        })).collect::<Vec<_>>(),
        "custom_tools": tool_defs.iter().map(|tool| &tool.name).collect::<Vec<_>>(),
        "entrypoint": loader.entrypoint().and_then(|entrypoint| {
            entrypoint.strip_prefix(&flow_path).ok()
                .map(|relative| relative.display().to_string())
                .or_else(|| entrypoint.file_name().map(|name| name.to_string_lossy().into_owned()))
        }),
    }))
}

pub(super) fn list_agents(
    state: &AppState,
    flow: String,
) -> Result<Vec<serde_json::Value>, InspectionFailure> {
    let (_, loader) = load_project(state, &flow)?;
    let agents = crate::lua::api::load_agents_from_files(loader.agent_files())
        .map_err(|error| (StatusCode::BAD_REQUEST, sanitize_error(&error)))?;
    Ok(agents
        .iter()
        .map(|agent| {
            serde_json::json!({
                "name": agent.name,
                "goal": agent.goal,
                "capabilities": agent.capabilities,
                "tools": agent.tools,
                "temperature": agent.temperature,
                "model": agent.model,
            })
        })
        .collect())
}
