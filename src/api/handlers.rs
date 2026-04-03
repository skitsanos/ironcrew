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
    body: Option<Json<serde_json::Value>>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    let flow_path = resolve_flow_path(&state, &flow)
        .map_err(|e| error_response(flow_status(&e), e.to_string()))?;

    let input = body.map(|Json(v)| v);

    let run_id = uuid::Uuid::new_v4().to_string();
    let eventbus = EventBus::new(256);

    // Spawn the actual work as a child task — store its AbortHandle for cancellation
    let eventbus_inner = eventbus.clone();
    let run_id_for_work = run_id.clone();
    let work_handle = tokio::spawn(async move {
        execute_crew_from_path_with_events(
            &flow_path,
            &eventbus_inner,
            &run_id_for_work,
            input.as_ref(),
        )
        .await
    });
    let abort_handle = work_handle.abort_handle();

    // Store eventbus + abort handle for SSE subscribers and abort endpoint
    state.active_runs.write().await.insert(
        run_id.clone(),
        super::ActiveRun {
            eventbus: eventbus.clone(),
            abort_handle,
        },
    );

    let run_id_clone = run_id.clone();
    let state_clone = state.clone();
    let flow_clone = flow.clone();

    // Monitor the work handle: emit RunComplete on finish, timeout, or abort
    tokio::spawn(async move {
        let max_lifetime_secs: u64 = std::env::var("IRONCREW_MAX_RUN_LIFETIME")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(30 * 60);
        let max_lifetime = std::time::Duration::from_secs(max_lifetime_secs);
        let mut work_handle = work_handle;

        // Race the work against the timeout
        tokio::select! {
            join_result = &mut work_handle => {
                // Small delay to drain final events
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;

                match join_result {
                    Ok(Ok(response)) => {
                        eventbus.emit(CrewEvent::RunComplete {
                            run_id: run_id_clone.clone(),
                            status: response.status.clone(),
                            duration_ms: response.duration_ms,
                            total_tokens: response.results.iter().map(|_| 0u32).sum(),
                        });
                    }
                    Ok(Err(e)) => {
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
                    Err(join_err) if join_err.is_cancelled() => {
                        // Aborted via the abort endpoint
                        eventbus.emit(CrewEvent::RunComplete {
                            run_id: run_id_clone.clone(),
                            status: "aborted".into(),
                            duration_ms: 0,
                            total_tokens: 0,
                        });
                    }
                    Err(join_err) => {
                        eventbus.emit(CrewEvent::Log {
                            level: "error".into(),
                            message: format!("Task panicked: {}", join_err),
                        });
                        eventbus.emit(CrewEvent::RunComplete {
                            run_id: run_id_clone.clone(),
                            status: "failed".into(),
                            duration_ms: 0,
                            total_tokens: 0,
                        });
                    }
                }
            }
            _ = tokio::time::sleep(max_lifetime) => {
                // Timeout: abort the work handle to cancel ongoing LLM calls
                // and any sub-tasks awaiting inside the orchestrator.
                work_handle.abort();
                tracing::warn!("Run {} timed out after {}s", run_id_clone, max_lifetime.as_secs());
                eventbus.emit(CrewEvent::RunComplete {
                    run_id: run_id_clone.clone(),
                    status: "timeout".into(),
                    duration_ms: max_lifetime.as_millis() as u64,
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

/// Execute a crew from a flow path, injecting an EventBus, run_id, and optional input context.
async fn execute_crew_from_path_with_events(
    flow_path: &std::path::Path,
    eventbus: &EventBus,
    run_id: &str,
    input: Option<&serde_json::Value>,
) -> std::result::Result<RunCrewResponse, IronCrewError> {
    use crate::cli::project::{load_project, setup_crew_runtime};
    use crate::lua::api::json_value_to_lua;

    let loader = load_project(flow_path)?;
    let (lua, _runtime) = setup_crew_runtime(&loader)?;

    // Store the eventbus in a Lua global so LuaCrew::run() can pick it up
    lua.set_app_data(eventbus.clone());

    // Store the run_id so LuaCrew::run() uses it for the RunRecord
    lua.set_app_data(run_id.to_string());

    // Inject input as a global `input` table (from the HTTP request body)
    if let Some(input_value) = input {
        // Extract tags from input if present (e.g., {"tags": ["v2", "experiment"], ...})
        if let Some(tags) = input_value.get("tags").and_then(|v| v.as_array()) {
            let tag_strings: Vec<String> = tags
                .iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect();
            if !tag_strings.is_empty() {
                lua.set_app_data(tag_strings);
            }
        }

        let lua_input = json_value_to_lua(&lua, input_value).map_err(IronCrewError::Lua)?;
        lua.globals()
            .set("input", lua_input)
            .map_err(IronCrewError::Lua)?;
    }

    // Execute the Lua script
    let entrypoint = loader
        .entrypoint()
        .ok_or_else(|| IronCrewError::Validation("No entrypoint found".into()))?;
    let script = std::fs::read_to_string(entrypoint)?;

    let exec_err = lua.load(&script).exec_async().await.err();

    // Even if post-run Lua code failed (e.g., json_parse on skipped output),
    // the crew may have completed successfully. Check the run record first.
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

    // No run record found — if the Lua script failed, propagate the error
    if let Some(err) = exec_err {
        return Err(IronCrewError::Lua(err));
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
// Abort a running crew
// ---------------------------------------------------------------------------

pub async fn abort_run(
    State(state): State<Arc<AppState>>,
    Path((_flow, run_id)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    let active_runs = state.active_runs.read().await;
    let active_run = active_runs.get(&run_id).ok_or_else(|| {
        error_response(
            StatusCode::NOT_FOUND,
            format!("Run '{}' not found or already completed", run_id),
        )
    })?;

    active_run.abort_handle.abort();
    tracing::info!("Run {} aborted by client", run_id);

    Ok(Json(serde_json::json!({
        "run_id": run_id,
        "status": "aborted",
    })))
}

// ---------------------------------------------------------------------------
// SSE event stream
// ---------------------------------------------------------------------------

fn event_type_str(event: &CrewEvent) -> &'static str {
    match event {
        CrewEvent::CrewStarted { .. } => "crew_started",
        CrewEvent::PhaseStart { .. } => "phase_start",
        CrewEvent::TaskAssigned { .. } => "task_assigned",
        CrewEvent::TaskCompleted { .. } => "task_completed",
        CrewEvent::TaskFailed { .. } => "task_failed",
        CrewEvent::TaskSkipped { .. } => "task_skipped",
        CrewEvent::TaskRetry { .. } => "task_retry",
        CrewEvent::ToolCall { .. } => "tool_call",
        CrewEvent::ToolResult { .. } => "tool_result",
        CrewEvent::MessageSent { .. } => "message_sent",
        CrewEvent::CollaborationTurn { .. } => "collaboration_turn",
        CrewEvent::MemorySet { .. } => "memory_set",
        CrewEvent::Log { .. } => "log",
        CrewEvent::RunComplete { .. } => "run_complete",
    }
}

/// Optionally truncate output fields in SSE events.
/// Returns the event with output capped at max_chars (if configured).
/// When disabled (default), returns the event unchanged.
fn truncate_event_output(event: CrewEvent, max_chars: Option<usize>) -> CrewEvent {
    let Some(max) = max_chars else {
        return event;
    };
    match event {
        CrewEvent::TaskCompleted {
            task,
            agent,
            duration_ms,
            success,
            output,
            token_usage,
        } if output.len() > max => CrewEvent::TaskCompleted {
            task,
            agent,
            duration_ms,
            success,
            output: format!(
                "{}... [truncated, {} total chars]",
                &output[..max],
                output.len()
            ),
            token_usage,
        },
        CrewEvent::CollaborationTurn {
            task,
            agent,
            turn,
            content,
        } if content.len() > max => CrewEvent::CollaborationTurn {
            task,
            agent,
            turn,
            content: format!(
                "{}... [truncated, {} total chars]",
                &content[..max],
                content.len()
            ),
        },
        other => other,
    }
}

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
    let active_run = active_runs.get(&run_id).ok_or_else(|| {
        error_response(
            StatusCode::NOT_FOUND,
            format!("Run '{}' not found or already completed", run_id),
        )
    })?;

    // Get replay buffer and live subscription
    let replay = active_run.eventbus.replay().await;
    let mut rx = active_run.eventbus.subscribe();
    drop(active_runs);

    // Optional output truncation (disabled by default)
    let sse_max_chars: Option<usize> = std::env::var("IRONCREW_SSE_OUTPUT_MAX_CHARS")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|&n| n > 0);

    let stream = async_stream::stream! {
        // First: replay all past events for late subscribers
        for event in replay {
            let event = truncate_event_output(event, sse_max_chars);
            let event_type = event_type_str(&event);
            let data = serde_json::to_string(&event).unwrap_or_default();
            yield Ok(Event::default().event(event_type).data(data));

            if matches!(event, CrewEvent::RunComplete { .. }) {
                return; // Run already finished, no need for live stream
            }
        }

        // Then: stream live events
        loop {
            match rx.recv().await {
                Ok(event) => {
                    let event = truncate_event_output(event, sse_max_chars);
                    let event_type = event_type_str(&event);
                    let data = serde_json::to_string(&event).unwrap_or_default();
                    yield Ok(Event::default().event(event_type).data(data));

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
