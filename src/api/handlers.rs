use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::Json,
    response::sse::{Event, Sse},
};
use std::convert::Infallible;
use std::sync::Arc;
use tokio::sync::broadcast;

use crate::engine::eventbus::{CrewEvent, EventBus};
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
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    let flow_path = resolve_flow_path(&state, &flow)
        .map_err(|e| error_response(flow_status(&e), e.to_string()))?;

    let run_id = uuid::Uuid::new_v4().to_string();
    let eventbus = EventBus::new(256);

    // Store for SSE subscribers
    state
        .active_runs
        .write()
        .await
        .insert(run_id.clone(), eventbus.clone());

    let run_id_clone = run_id.clone();
    let state_clone = state.clone();
    let flow_clone = flow.clone();

    // Spawn execution in background
    tokio::spawn(async move {
        let result = execute_crew_from_path_with_events(&flow_path, &eventbus).await;

        match result {
            Ok(response) => {
                eventbus.emit(CrewEvent::RunComplete {
                    run_id: response.run_id.clone(),
                    status: response.status.clone(),
                    duration_ms: response.duration_ms,
                    total_tokens: response.results.iter().map(|_| 0u32).sum(),
                });
            }
            Err(e) => {
                eventbus.emit(CrewEvent::Log {
                    level: "error".into(),
                    message: e.to_string(),
                });
                eventbus.emit(CrewEvent::RunComplete {
                    run_id: run_id_clone.clone(),
                    status: "failed".into(),
                    duration_ms: 0,
                    total_tokens: 0,
                });
            }
        }

        // Clean up after a delay to allow SSE clients to receive the final event
        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        state_clone.active_runs.write().await.remove(&run_id_clone);
    });

    Ok(Json(serde_json::json!({
        "run_id": run_id,
        "status": "started",
        "events_url": format!("/flows/{}/events/{}", flow_clone, run_id),
    })))
}

/// Execute a crew from a flow path, injecting an EventBus so the orchestrator emits events.
async fn execute_crew_from_path_with_events(
    flow_path: &std::path::Path,
    eventbus: &EventBus,
) -> std::result::Result<RunCrewResponse, IronCrewError> {
    use crate::cli::project::{load_project, setup_crew_runtime};

    let loader = load_project(flow_path)?;
    let (lua, _runtime) = setup_crew_runtime(&loader)?;

    // Store the eventbus in a Lua global so LuaCrew::run() can pick it up
    lua.set_app_data(eventbus.clone());

    // Execute
    let entrypoint = loader
        .entrypoint()
        .ok_or_else(|| IronCrewError::Validation("No entrypoint found".into()))?;
    let script = std::fs::read_to_string(entrypoint)?;

    lua.load(&script)
        .exec_async()
        .await
        .map_err(IronCrewError::Lua)?;

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

/// Original synchronous-style execution (kept for backward compatibility / CLI use).
#[allow(dead_code)]
pub async fn execute_crew_from_path(
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
// SSE event stream
// ---------------------------------------------------------------------------

pub async fn flow_events(
    State(state): State<Arc<AppState>>,
    Path((flow, run_id)): Path<(String, String)>,
) -> Result<
    Sse<impl futures::stream::Stream<Item = std::result::Result<Event, Infallible>>>,
    (StatusCode, Json<ErrorResponse>),
> {
    // Validate flow exists
    let _ = resolve_flow_path(&state, &flow)
        .map_err(|e| error_response(flow_status(&e), e.to_string()))?;

    let active_runs = state.active_runs.read().await;
    let eventbus = active_runs.get(&run_id).ok_or_else(|| {
        error_response(
            StatusCode::NOT_FOUND,
            format!("Run '{}' not found or already completed", run_id),
        )
    })?;

    let mut rx = eventbus.subscribe();
    drop(active_runs);

    let stream = async_stream::stream! {
        loop {
            match rx.recv().await {
                Ok(event) => {
                    let event_type = match &event {
                        CrewEvent::PhaseStart { .. } => "phase_start",
                        CrewEvent::TaskAssigned { .. } => "task_assigned",
                        CrewEvent::TaskCompleted { .. } => "task_completed",
                        CrewEvent::TaskFailed { .. } => "task_failed",
                        CrewEvent::TaskSkipped { .. } => "task_skipped",
                        CrewEvent::ToolCall { .. } => "tool_call",
                        CrewEvent::Log { .. } => "log",
                        CrewEvent::RunComplete { .. } => "run_complete",
                    };

                    let data = serde_json::to_string(&event).unwrap_or_default();
                    let sse_event = Event::default().event(event_type).data(data);
                    yield Ok(sse_event);

                    // Close stream after run_complete
                    if matches!(event, CrewEvent::RunComplete { .. }) {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    let sse_event = Event::default()
                        .event("warning")
                        .data(format!("{{\"message\":\"missed {} events\"}}", n));
                    yield Ok(sse_event);
                }
                Err(broadcast::error::RecvError::Closed) => {
                    break;
                }
            }
        }
    };

    Ok(Sse::new(stream))
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
        file_read::FileReadTool, file_read_glob::FileReadGlobTool, file_write::FileWriteTool,
        hash::HashTool, http_request::HttpRequestTool, shell::ShellTool,
        template_render::TemplateRenderTool, validate_schema::ValidateSchemaTool,
        web_scrape::WebScrapeTool,
    };

    let mut registry = ToolRegistry::new();
    registry.register(Box::new(FileReadTool::new(None)));
    registry.register(Box::new(FileReadGlobTool::new(None)));
    registry.register(Box::new(FileWriteTool::new(None, None)));
    registry.register(Box::new(WebScrapeTool::new(None)));
    registry.register(Box::new(ShellTool::new()));
    registry.register(Box::new(HttpRequestTool::new()));
    registry.register(Box::new(HashTool::new()));
    registry.register(Box::new(TemplateRenderTool::new()));
    registry.register(Box::new(ValidateSchemaTool::new()));

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
