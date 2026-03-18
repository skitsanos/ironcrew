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
    pub store_dir: PathBuf,
}

/// Request to run a crew
#[derive(Deserialize)]
pub struct RunCrewRequest {
    /// Path to the crew file or directory (relative to flows_dir)
    pub flow: String,
    /// Optional initial context as JSON
    #[allow(dead_code)]
    pub context: Option<serde_json::Value>,
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

/// Build the router
pub fn create_router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/flows/run", post(run_crew))
        .route("/runs", get(list_runs))
        .route("/runs/{id}", get(get_run))
        .route("/runs/{id}", delete(delete_run))
        .route("/nodes", get(list_nodes))
        .with_state(state)
}

async fn health() -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
    }))
}

async fn run_crew(
    State(state): State<Arc<AppState>>,
    Json(req): Json<RunCrewRequest>,
) -> Result<Json<RunCrewResponse>, (StatusCode, Json<ErrorResponse>)> {
    let flow_path = state.flows_dir.join(&req.flow);

    if !flow_path.exists() {
        return Err(error_response(
            StatusCode::NOT_FOUND,
            format!("Flow not found: {}", req.flow),
        ));
    }

    // Execute the crew
    let result = execute_crew_from_path(&flow_path, &state.store_dir).await;

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
    _store_dir: &std::path::Path,
) -> std::result::Result<RunCrewResponse, IronCrewError> {
    use crate::engine::runtime::Runtime;
    use crate::llm::openai::OpenAiProvider;
    use crate::lua::api::*;
    use crate::lua::loader::ProjectLoader;
    use crate::lua::sandbox::create_crew_lua;

    // Load .env
    dotenvy::dotenv().ok();
    let project_dir = if flow_path.is_file() {
        flow_path.parent().unwrap_or(std::path::Path::new("."))
    } else {
        flow_path
    };
    let env_file = project_dir.join(".env");
    if env_file.exists() {
        dotenvy::from_path(&env_file).ok();
    }

    // Load project
    let loader = if flow_path.is_file() {
        ProjectLoader::from_file(flow_path)?
    } else {
        ProjectLoader::from_directory(flow_path)?
    };

    let lua = create_crew_lua().map_err(IronCrewError::Lua)?;
    register_agent_constructor(&lua).map_err(IronCrewError::Lua)?;

    // Create provider
    let api_key = std::env::var("OPENAI_API_KEY").map_err(|_| {
        IronCrewError::Validation("OPENAI_API_KEY environment variable not set".into())
    })?;
    let base_url = std::env::var("OPENAI_BASE_URL").ok();
    let provider: Box<dyn crate::llm::provider::LlmProvider> =
        Box::new(OpenAiProvider::new(api_key, base_url));

    // Load agents and tools
    let preloaded_agents = load_agents_from_files(&lua, loader.agent_files())?;
    let tool_defs = load_tool_defs_from_files(&lua, loader.tool_files())?;

    let mut runtime = Runtime::new(provider, Some(loader.project_dir()));
    runtime.register_lua_tools(tool_defs);
    let runtime = Arc::new(runtime);

    register_crew_constructor(
        &lua,
        runtime.clone(),
        preloaded_agents,
        loader.project_dir().to_path_buf(),
    )
    .map_err(IronCrewError::Lua)?;

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
    let runs_dir = project_dir.join(".ironcrew").join("runs");
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

    // If no history available, return a minimal response
    Ok(RunCrewResponse {
        run_id: uuid::Uuid::new_v4().to_string(),
        flow_name: "unknown".into(),
        status: "completed".into(),
        duration_ms: 0,
        results: vec![],
    })
}

async fn list_runs(
    State(state): State<Arc<AppState>>,
    Query(params): Query<ListRunsQuery>,
) -> Result<Json<Vec<RunRecord>>, (StatusCode, Json<ErrorResponse>)> {
    let history = RunHistory::new(state.store_dir.clone())
        .map_err(|e| error_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let runs = history
        .list(params.status.as_deref())
        .map_err(|e| error_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(runs))
}

async fn get_run(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<RunRecord>, (StatusCode, Json<ErrorResponse>)> {
    let history = RunHistory::new(state.store_dir.clone())
        .map_err(|e| error_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let record = history
        .get(&id)
        .map_err(|e| error_response(StatusCode::NOT_FOUND, e.to_string()))?;

    Ok(Json(record))
}

async fn delete_run(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    let history = RunHistory::new(state.store_dir.clone())
        .map_err(|e| error_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    history
        .delete(&id)
        .map_err(|e| error_response(StatusCode::NOT_FOUND, e.to_string()))?;

    Ok(Json(serde_json::json!({"deleted": id})))
}

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
