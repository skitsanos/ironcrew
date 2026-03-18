use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::Json,
    routing::{delete, get, post},
    Router,
};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;

use crate::engine::run_history::{RunHistory, RunRecord};
use crate::utils::error::IronCrewError;

/// Shared application state
pub struct AppState {
    pub flows_dir: PathBuf,
}

/// Response from running a crew
#[derive(Serialize)]
pub struct RunCrewResponse {
    pub run_id: String,
    pub flow_name: String,
    pub status: String,
    pub duration_ms: u64,
    pub results: Vec<TaskResultResponse>,
}

#[derive(Serialize)]
pub struct TaskResultResponse {
    pub task: String,
    pub agent: String,
    pub output: String,
    pub success: bool,
    pub duration_ms: u64,
}

/// Query params for listing runs
#[derive(Deserialize)]
pub struct ListRunsQuery {
    pub status: Option<String>,
}

/// Error response
#[derive(Serialize)]
pub struct ErrorResponse {
    pub error: String,
}

fn error_response(status: StatusCode, message: String) -> (StatusCode, Json<ErrorResponse>) {
    (status, Json(ErrorResponse { error: message }))
}

/// Resolve the runs directory for a given flow.
fn resolve_runs_dir(state: &AppState, flow: &str) -> PathBuf {
    state.flows_dir.join(flow).join(".ironcrew").join("runs")
}

/// Build the router
pub fn create_router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/flows/{flow}/run", post(run_flow))
        .route("/flows/{flow}/runs", get(list_runs))
        .route("/flows/{flow}/runs/{id}", get(get_run))
        .route("/flows/{flow}/runs/{id}", delete(delete_run))
        .route("/flows/{flow}/validate", get(validate_flow))
        .route("/flows/{flow}/agents", get(list_agents))
        .route("/nodes", get(list_nodes))
        .with_state(state)
}

async fn health() -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
    }))
}

// ---------------------------------------------------------------------------
// Flow execution
// ---------------------------------------------------------------------------

async fn run_flow(
    State(state): State<Arc<AppState>>,
    Path(flow): Path<String>,
) -> Result<Json<RunCrewResponse>, (StatusCode, Json<ErrorResponse>)> {
    let flow_path = state.flows_dir.join(&flow);

    if !flow_path.exists() {
        return Err(error_response(
            StatusCode::NOT_FOUND,
            format!("Flow not found: {}", flow),
        ));
    }

    let result = execute_crew_from_path(&flow_path).await;

    match result {
        Ok(response) => Ok(Json(response)),
        Err(e) => Err(error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            e.to_string(),
        )),
    }
}

async fn execute_crew_from_path(
    flow_path: &std::path::Path,
) -> std::result::Result<RunCrewResponse, IronCrewError> {
    use crate::cli::project::{load_project, setup_crew_runtime};

    let loader = load_project(flow_path)?;
    let (lua, _runtime) = setup_crew_runtime(&loader)?;

    // Execute
    let entrypoint = loader
        .entrypoint()
        .ok_or_else(|| IronCrewError::Validation("No entrypoint found".into()))?;
    let script = std::fs::read_to_string(entrypoint)?;

    lua.load(&script)
        .exec_async()
        .await
        .map_err(IronCrewError::Lua)?;

    // Read the last run from history to get results
    let runs_dir = loader.project_dir().join(".ironcrew").join("runs");
    if let Ok(history) = RunHistory::new(runs_dir)
        && let Ok(runs) = history.list(None)
        && let Some(latest) = runs.first()
    {
        return Ok(RunCrewResponse {
            run_id: latest.run_id.clone(),
            flow_name: latest.flow_name.clone(),
            status: latest.status.to_string(),
            duration_ms: latest.duration_ms,
            results: latest
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

async fn list_runs(
    State(state): State<Arc<AppState>>,
    Path(flow): Path<String>,
    Query(params): Query<ListRunsQuery>,
) -> Result<Json<Vec<RunRecord>>, (StatusCode, Json<ErrorResponse>)> {
    let runs_dir = resolve_runs_dir(&state, &flow);
    let history = RunHistory::new(runs_dir)
        .map_err(|e| error_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let runs = history
        .list(params.status.as_deref())
        .map_err(|e| error_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(runs))
}

async fn get_run(
    State(state): State<Arc<AppState>>,
    Path((flow, id)): Path<(String, String)>,
) -> Result<Json<RunRecord>, (StatusCode, Json<ErrorResponse>)> {
    let runs_dir = resolve_runs_dir(&state, &flow);
    let history = RunHistory::new(runs_dir)
        .map_err(|e| error_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let record = history
        .get(&id)
        .map_err(|e| error_response(StatusCode::NOT_FOUND, e.to_string()))?;

    Ok(Json(record))
}

async fn delete_run(
    State(state): State<Arc<AppState>>,
    Path((flow, id)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    let runs_dir = resolve_runs_dir(&state, &flow);
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

async fn validate_flow(
    State(state): State<Arc<AppState>>,
    Path(flow): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    let flow_path = state.flows_dir.join(&flow);
    if !flow_path.exists() {
        return Err(error_response(
            StatusCode::NOT_FOUND,
            format!("Flow not found: {}", flow),
        ));
    }

    use crate::lua::api::*;
    use crate::lua::loader::ProjectLoader;
    use crate::lua::sandbox::create_crew_lua;

    let loader = if flow_path.is_file() {
        ProjectLoader::from_file(&flow_path)
    } else {
        ProjectLoader::from_directory(&flow_path)
    }
    .map_err(|e| error_response(StatusCode::BAD_REQUEST, e.to_string()))?;

    let lua = create_crew_lua()
        .map_err(|e| error_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let agents = load_agents_from_files(&lua, loader.agent_files()).unwrap_or_default();
    let tool_defs = load_tool_defs_from_files(&lua, loader.tool_files()).unwrap_or_default();

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

async fn list_agents(
    State(state): State<Arc<AppState>>,
    Path(flow): Path<String>,
) -> Result<Json<Vec<serde_json::Value>>, (StatusCode, Json<ErrorResponse>)> {
    let flow_path = state.flows_dir.join(&flow);
    if !flow_path.exists() {
        return Err(error_response(
            StatusCode::NOT_FOUND,
            format!("Flow not found: {}", flow),
        ));
    }

    use crate::lua::api::*;
    use crate::lua::loader::ProjectLoader;
    use crate::lua::sandbox::create_crew_lua;

    let loader = if flow_path.is_file() {
        ProjectLoader::from_file(&flow_path)
    } else {
        ProjectLoader::from_directory(&flow_path)
    }
    .map_err(|e| error_response(StatusCode::BAD_REQUEST, e.to_string()))?;

    let lua = create_crew_lua()
        .map_err(|e| error_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let agents = load_agents_from_files(&lua, loader.agent_files()).unwrap_or_default();

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

async fn list_nodes() -> Json<Vec<serde_json::Value>> {
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
