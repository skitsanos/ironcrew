use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::Json,
};
use std::sync::Arc;

use crate::engine::run_history::RunHistory;
use crate::utils::error::IronCrewError;

use super::{
    AppState, ErrorResponse, ListRunsQuery, RunCrewResponse, TaskResultResponse, error_response,
    resolve_flow_path, resolve_runs_dir,
};

fn flow_status(err: &IronCrewError) -> StatusCode {
    if err.to_string().contains("not found") {
        StatusCode::NOT_FOUND
    } else {
        StatusCode::BAD_REQUEST
    }
}

pub async fn health() -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
    }))
}

// ---------------------------------------------------------------------------
// Flow execution
// ---------------------------------------------------------------------------

pub async fn run_flow(
    State(state): State<Arc<AppState>>,
    Path(flow): Path<String>,
) -> Result<Json<RunCrewResponse>, (StatusCode, Json<ErrorResponse>)> {
    let flow_path = resolve_flow_path(&state, &flow)
        .map_err(|e| error_response(flow_status(&e), e.to_string()))?;

    let result = execute_crew_from_path(&flow_path).await;

    match result {
        Ok(response) => Ok(Json(response)),
        Err(e) => Err(error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            e.to_string(),
        )),
    }
}

pub async fn execute_crew_from_path(
    flow_path: &std::path::Path,
) -> std::result::Result<RunCrewResponse, crate::utils::error::IronCrewError> {
    use crate::cli::project::{load_project, setup_crew_runtime};

    let loader = load_project(flow_path)?;
    let (lua, _runtime) = setup_crew_runtime(&loader)?;

    // Execute
    let entrypoint = loader.entrypoint().ok_or_else(|| {
        crate::utils::error::IronCrewError::Validation("No entrypoint found".into())
    })?;
    let script = std::fs::read_to_string(entrypoint)?;

    lua.load(&script)
        .exec_async()
        .await
        .map_err(crate::utils::error::IronCrewError::Lua)?;

    let run_id: Option<String> = lua.globals().get("__ironcrew_last_run_id").ok();

    // Read the recorded run directly so concurrent executions cannot swap results.
    let runs_dir = loader.project_dir().join(".ironcrew").join("runs");
    if let Some(run_id) = run_id {
        let history = RunHistory::new(runs_dir)?;
        let run = history.get(&run_id)?;
        return Ok(RunCrewResponse {
            run_id: run.run_id.clone(),
            flow_name: run.flow_name.clone(),
            status: run.status.to_string(),
            duration_ms: run.duration_ms,
            results: run
                .task_results
                .iter()
                .map(|r| TaskResultResponse {
                    task: r.task.clone(),
                    agent: r.agent.clone(),
                    output: r.output.clone(),
                    success: r.success,
                    duration_ms: r.duration_ms,
                })
                .collect(),
        });
    }

    Ok(RunCrewResponse {
        run_id: uuid::Uuid::new_v4().to_string(),
        flow_name: "unknown".into(),
        status: "completed".into(),
        duration_ms: 0,
        results: vec![],
    })
}

// ---------------------------------------------------------------------------
// Run history (per-flow)
// ---------------------------------------------------------------------------

pub async fn list_runs(
    State(state): State<Arc<AppState>>,
    Path(flow): Path<String>,
    Query(params): Query<ListRunsQuery>,
) -> Result<Json<Vec<crate::engine::run_history::RunRecord>>, (StatusCode, Json<ErrorResponse>)> {
    let runs_dir = resolve_runs_dir(&state, &flow)
        .map_err(|e| error_response(flow_status(&e), e.to_string()))?;
    let history = RunHistory::new(runs_dir)
        .map_err(|e| error_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let runs = history
        .list(params.status.as_deref())
        .map_err(|e| error_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(runs))
}

pub async fn get_run(
    State(state): State<Arc<AppState>>,
    Path((flow, id)): Path<(String, String)>,
) -> Result<Json<crate::engine::run_history::RunRecord>, (StatusCode, Json<ErrorResponse>)> {
    let runs_dir = resolve_runs_dir(&state, &flow)
        .map_err(|e| error_response(flow_status(&e), e.to_string()))?;
    let history = RunHistory::new(runs_dir)
        .map_err(|e| error_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let record = history
        .get(&id)
        .map_err(|e| error_response(StatusCode::NOT_FOUND, e.to_string()))?;

    Ok(Json(record))
}

pub async fn delete_run(
    State(state): State<Arc<AppState>>,
    Path((flow, id)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    let runs_dir = resolve_runs_dir(&state, &flow)
        .map_err(|e| error_response(flow_status(&e), e.to_string()))?;
    let history = RunHistory::new(runs_dir)
        .map_err(|e| error_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    history
        .delete(&id)
        .map_err(|e| error_response(StatusCode::NOT_FOUND, e.to_string()))?;

    Ok(Json(serde_json::json!({"deleted": id})))
}

// ---------------------------------------------------------------------------
// Flow inspection
// ---------------------------------------------------------------------------

pub async fn validate_flow(
    State(state): State<Arc<AppState>>,
    Path(flow): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    let flow_path = resolve_flow_path(&state, &flow)
        .map_err(|e| error_response(flow_status(&e), e.to_string()))?;

    use crate::lua::api::*;
    use crate::lua::loader::ProjectLoader;
    use crate::lua::sandbox::create_tool_lua;

    let loader = if flow_path.is_file() {
        ProjectLoader::from_file(&flow_path)
    } else {
        ProjectLoader::from_directory(&flow_path)
    }
    .map_err(|e| error_response(StatusCode::BAD_REQUEST, e.to_string()))?;

    let lua = create_tool_lua()
        .map_err(|e| error_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let agents = load_agents_from_files(loader.agent_files())
        .map_err(|e| error_response(StatusCode::BAD_REQUEST, e.to_string()))?;
    let tool_defs = load_tool_defs_from_files(loader.tool_files())
        .map_err(|e| error_response(StatusCode::BAD_REQUEST, e.to_string()))?;

    // Check entrypoint syntax
    let entrypoint_valid = if let Some(ep) = loader.entrypoint() {
        if let Ok(script) = std::fs::read_to_string(ep) {
            lua.load(&script).into_function().is_ok()
        } else {
            false
        }
    } else {
        false
    };

    Ok(Json(serde_json::json!({
        "flow": flow,
        "valid": entrypoint_valid,
        "agents": agents.iter().map(|a| serde_json::json!({
            "name": a.name,
            "goal": a.goal,
            "capabilities": a.capabilities,
            "tools": a.tools,
        })).collect::<Vec<_>>(),
        "custom_tools": tool_defs.iter().map(|t| &t.name).collect::<Vec<_>>(),
        "entrypoint": loader.entrypoint().map(|p| p.display().to_string()),
    })))
}

pub async fn list_agents(
    State(state): State<Arc<AppState>>,
    Path(flow): Path<String>,
) -> Result<Json<Vec<serde_json::Value>>, (StatusCode, Json<ErrorResponse>)> {
    let flow_path = resolve_flow_path(&state, &flow)
        .map_err(|e| error_response(flow_status(&e), e.to_string()))?;

    use crate::lua::api::*;
    use crate::lua::loader::ProjectLoader;

    let loader = if flow_path.is_file() {
        ProjectLoader::from_file(&flow_path)
    } else {
        ProjectLoader::from_directory(&flow_path)
    }
    .map_err(|e| error_response(StatusCode::BAD_REQUEST, e.to_string()))?;

    let agents = load_agents_from_files(loader.agent_files())
        .map_err(|e| error_response(StatusCode::BAD_REQUEST, e.to_string()))?;

    let result: Vec<serde_json::Value> = agents
        .iter()
        .map(|a| {
            serde_json::json!({
                "name": a.name,
                "goal": a.goal,
                "capabilities": a.capabilities,
                "tools": a.tools,
                "temperature": a.temperature,
                "model": a.model,
            })
        })
        .collect();

    Ok(Json(result))
}

// ---------------------------------------------------------------------------
// Nodes (global)
// ---------------------------------------------------------------------------

pub async fn list_nodes() -> Json<Vec<serde_json::Value>> {
    use crate::tools::registry::ToolRegistry;
    use crate::tools::{
        file_read::FileReadTool, file_write::FileWriteTool, hash::HashTool,
        http_request::HttpRequestTool, shell::ShellTool, template_render::TemplateRenderTool,
        web_scrape::WebScrapeTool,
    };

    let mut registry = ToolRegistry::new();
    registry.register(Box::new(FileReadTool::new(None)));
    registry.register(Box::new(FileWriteTool::new(None, None)));
    registry.register(Box::new(WebScrapeTool::new(None)));
    registry.register(Box::new(ShellTool::new()));
    registry.register(Box::new(HttpRequestTool::new()));
    registry.register(Box::new(HashTool::new()));
    registry.register(Box::new(TemplateRenderTool::new()));

    let mut tools: Vec<serde_json::Value> = Vec::new();
    let mut names = registry.list();
    names.sort();

    for name in &names {
        if let Some(tool) = registry.get(name) {
            tools.push(serde_json::json!({
                "name": name,
                "description": tool.description(),
                "schema": tool.schema().parameters,
            }));
        }
    }

    Json(tools)
}
