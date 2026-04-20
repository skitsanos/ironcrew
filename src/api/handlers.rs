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
use crate::engine::store::create_store;
use crate::utils::error::IronCrewError;

use super::{
    AppState, ErrorResponse, ListRunsQuery, ListRunsResponse, RunCrewResponse, TaskResultResponse,
    error_response, resolve_flow_path, resolve_ironcrew_dir,
};

fn flow_status(err: &IronCrewError) -> StatusCode {
    if err.to_string().contains("not found") {
        StatusCode::NOT_FOUND
    } else {
        StatusCode::BAD_REQUEST
    }
}

/// Sanitize an error for API responses: log the full detail, return a safe message.
/// Strips filesystem paths and internal details that could leak server structure.
fn sanitize_error(err: &IronCrewError) -> String {
    let full = err.to_string();
    tracing::warn!("API error: {}", full);

    // Keep validation messages that don't contain paths
    match err {
        IronCrewError::Validation(msg) => {
            // Strip anything that looks like an absolute path
            if msg.contains('/') || msg.contains('\\') {
                // Return just the high-level message
                if msg.contains("not found") {
                    "Resource not found".into()
                } else if msg.contains("Invalid flow") {
                    "Invalid flow identifier".into()
                } else {
                    "Invalid request".into()
                }
            } else {
                msg.clone()
            }
        }
        IronCrewError::Io(_) => "Internal storage error".into(),
        _ => "Internal server error".into(),
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
        .map_err(|e| error_response(flow_status(&e), sanitize_error(&e)))?;

    let input = body.map(|Json(v)| v);

    let run_id = uuid::Uuid::new_v4().to_string();
    let eventbus = EventBus::new(256);

    // Spawn the actual work as a child task — store its AbortHandle for cancellation
    let eventbus_inner = eventbus.clone();
    let run_id_for_work = run_id.clone();
    let store_for_work = state.store.clone();
    let work_handle = tokio::spawn(async move {
        execute_crew_from_path_with_events(
            &flow_path,
            &eventbus_inner,
            &run_id_for_work,
            input.as_ref(),
            Some(store_for_work),
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
                            total_tokens: response.total_tokens,
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

        // Clean up after a short delay to allow SSE clients to receive the final event
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
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
    shared_store: Option<Arc<dyn crate::engine::store::StateStore>>,
) -> std::result::Result<RunCrewResponse, IronCrewError> {
    use crate::cli::project::{load_project, setup_crew_runtime};
    use crate::lua::api::json_value_to_lua;

    let loader = load_project(flow_path)?;
    let (lua, _runtime) = setup_crew_runtime(&loader)?;

    // Store the eventbus in a Lua global so LuaCrew::run() can pick it up
    lua.set_app_data(eventbus.clone());

    // Store the run_id so LuaCrew::run() uses it for the RunRecord
    lua.set_app_data(run_id.to_string());

    // Inject the server-wide store singleton so `LuaCrew` prefills its
    // OnceCell instead of bootstrapping a new Postgres pool per run.
    if let Some(store) = shared_store.clone() {
        lua.set_app_data(store);
    }

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
    if let Some(run_id) = run_id {
        let store = match shared_store.clone() {
            Some(s) => s,
            None => create_store(loader.project_dir().join(".ironcrew")).await?,
        };
        let run = store.get_run(&run_id).await?;
        return Ok(RunCrewResponse {
            run_id: run.run_id.clone(),
            flow_name: run.flow_name.clone(),
            status: run.status.to_string(),
            duration_ms: run.duration_ms,
            total_tokens: run.total_tokens,
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
        total_tokens: 0,
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
    let ironcrew_dir = loader.project_dir().join(".ironcrew");
    if let Some(run_id) = run_id {
        let store = create_store(ironcrew_dir).await?;
        let run = store.get_run(&run_id).await?;
        return Ok(RunCrewResponse {
            run_id: run.run_id.clone(),
            flow_name: run.flow_name.clone(),
            status: run.status.to_string(),
            duration_ms: run.duration_ms,
            total_tokens: run.total_tokens,
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
        total_tokens: 0,
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
        CrewEvent::TaskThinking { .. } => "task_thinking",
        CrewEvent::TaskRetry { .. } => "task_retry",
        CrewEvent::ToolCall { .. } => "tool_call",
        CrewEvent::ToolResult { .. } => "tool_result",
        CrewEvent::AgentToolStarted { .. } => "agent_tool_started",
        CrewEvent::AgentToolCompleted { .. } => "agent_tool_completed",
        CrewEvent::MessageSent { .. } => "message_sent",
        CrewEvent::CollaborationTurn { .. } => "collaboration_turn",
        CrewEvent::ConversationStarted { .. } => "conversation_started",
        CrewEvent::ConversationTurn { .. } => "conversation_turn",
        CrewEvent::ConversationThinking { .. } => "conversation_thinking",
        CrewEvent::DialogStarted { .. } => "dialog_started",
        CrewEvent::DialogTurn { .. } => "dialog_turn",
        CrewEvent::DialogThinking { .. } => "dialog_thinking",
        CrewEvent::DialogCompleted { .. } => "dialog_completed",
        CrewEvent::MemorySet { .. } => "memory_set",
        CrewEvent::Log { .. } => "log",
        CrewEvent::RunComplete { .. } => "run_complete",
    }
}

/// Truncate a string at the nearest UTF-8 char boundary at or below `max` bytes.
/// Returns a slice that is never in the middle of a multi-byte codepoint.
fn truncate_utf8(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut boundary = max;
    while boundary > 0 && !s.is_char_boundary(boundary) {
        boundary -= 1;
    }
    &s[..boundary]
}

/// Optionally truncate output fields in SSE events.
/// Works with Arc<CrewEvent> — returns a new owned event only when truncation is needed.
fn maybe_truncate_event(event: &CrewEvent, max_chars: Option<usize>) -> Option<CrewEvent> {
    let max = max_chars?;
    match event {
        CrewEvent::TaskCompleted {
            task,
            agent,
            duration_ms,
            success,
            output,
            token_usage,
        } if output.len() > max => Some(CrewEvent::TaskCompleted {
            task: task.clone(),
            agent: agent.clone(),
            duration_ms: *duration_ms,
            success: *success,
            output: format!(
                "{}... [truncated, {} total bytes]",
                truncate_utf8(output, max),
                output.len()
            ),
            token_usage: token_usage.clone(),
        }),
        CrewEvent::CollaborationTurn {
            task,
            agent,
            turn,
            content,
        } if content.len() > max => Some(CrewEvent::CollaborationTurn {
            task: task.clone(),
            agent: agent.clone(),
            turn: *turn,
            content: format!(
                "{}... [truncated, {} total bytes]",
                truncate_utf8(content, max),
                content.len()
            ),
        }),
        _ => None,
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
        .map_err(|e| error_response(flow_status(&e), sanitize_error(&e)))?;

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
            let effective = maybe_truncate_event(&event, sse_max_chars);
            let ev = effective.as_ref().unwrap_or(&event);
            let event_type = event_type_str(ev);
            let data = serde_json::to_string(ev).unwrap_or_default();
            yield Ok(Event::default().event(event_type).data(data));

            if matches!(ev, CrewEvent::RunComplete { .. }) {
                return; // Run already finished, no need for live stream
            }
        }

        // Then: stream live events
        loop {
            match rx.recv().await {
                Ok(event) => {
                    let effective = maybe_truncate_event(&event, sse_max_chars);
                    let ev = effective.as_ref().unwrap_or(&event);
                    let event_type = event_type_str(ev);
                    let data = serde_json::to_string(ev).unwrap_or_default();
                    yield Ok(Event::default().event(event_type).data(data));

                    if matches!(ev, CrewEvent::RunComplete { .. }) {
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

/// Default page size for `GET /flows/{flow}/runs` — override with `IRONCREW_RUNS_DEFAULT_LIMIT`.
fn runs_default_limit() -> usize {
    std::env::var("IRONCREW_RUNS_DEFAULT_LIMIT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(20)
}

/// Hard cap on page size — override with `IRONCREW_RUNS_MAX_LIMIT`.
/// A client that asks for more than this gets silently clamped.
fn runs_max_limit() -> usize {
    std::env::var("IRONCREW_RUNS_MAX_LIMIT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(100)
}

pub async fn list_runs(
    State(state): State<Arc<AppState>>,
    Path(flow): Path<String>,
    Query(params): Query<ListRunsQuery>,
) -> Result<Json<ListRunsResponse>, (StatusCode, Json<ErrorResponse>)> {
    // Flow path is still validated for traversal safety, but the store
    // itself is the server-wide singleton from `AppState`.
    let _ = resolve_ironcrew_dir(&state, &flow)
        .map_err(|e| error_response(flow_status(&e), sanitize_error(&e)))?;
    let store = &state.store;

    let default_limit = runs_default_limit();
    let max_limit = runs_max_limit();
    let limit = params.limit.unwrap_or(default_limit).min(max_limit).max(1);
    let offset = params.offset.unwrap_or(0);

    let filter = crate::engine::run_history::ListRunsFilter {
        status: params.status.clone(),
        tag: params.tag.clone(),
        since: params.since.clone(),
    };

    let runs = store
        .list_runs_summary(&filter, limit, offset)
        .await
        .map_err(|e| error_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let total = store
        .count_runs(&filter)
        .await
        .map_err(|e| error_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(ListRunsResponse {
        runs,
        total,
        limit,
        offset,
    }))
}

pub async fn get_run(
    State(state): State<Arc<AppState>>,
    Path((flow, id)): Path<(String, String)>,
) -> Result<Json<crate::engine::run_history::RunRecord>, (StatusCode, Json<ErrorResponse>)> {
    let _ = resolve_ironcrew_dir(&state, &flow)
        .map_err(|e| error_response(flow_status(&e), sanitize_error(&e)))?;

    let record = state
        .store
        .get_run(&id)
        .await
        .map_err(|e| error_response(StatusCode::NOT_FOUND, e.to_string()))?;

    Ok(Json(record))
}

pub async fn delete_run(
    State(state): State<Arc<AppState>>,
    Path((flow, id)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    let _ = resolve_ironcrew_dir(&state, &flow)
        .map_err(|e| error_response(flow_status(&e), sanitize_error(&e)))?;

    state
        .store
        .delete_run(&id)
        .await
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
        .map_err(|e| error_response(flow_status(&e), sanitize_error(&e)))?;

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
        .map_err(|e| error_response(flow_status(&e), sanitize_error(&e)))?;

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

#[cfg(test)]
mod truncate_tests {
    use super::truncate_utf8;

    #[test]
    fn ascii_under_limit_returns_full() {
        assert_eq!(truncate_utf8("hello", 10), "hello");
    }

    #[test]
    fn ascii_over_limit_truncates() {
        assert_eq!(truncate_utf8("hello world", 5), "hello");
    }

    #[test]
    fn emoji_truncate_does_not_panic() {
        // "🎉" is 4 bytes in UTF-8
        let s = "🎉🎉🎉🎉🎉"; // 20 bytes
        // Try every possible max from 0 to len — no panics
        for max in 0..=s.len() {
            let _ = truncate_utf8(s, max);
        }
    }

    #[test]
    fn emoji_truncate_lands_on_boundary() {
        let s = "🎉🎉🎉"; // 12 bytes, 3 chars
        // max=5 should walk back to boundary 4 (after first emoji)
        assert_eq!(truncate_utf8(s, 5), "🎉");
        // max=4 already a boundary
        assert_eq!(truncate_utf8(s, 4), "🎉");
        // max=3 walks back to 0
        assert_eq!(truncate_utf8(s, 3), "");
    }

    #[test]
    fn cjk_truncate_does_not_panic() {
        // CJK chars are 3 bytes each
        let s = "你好世界"; // 12 bytes, 4 chars
        for max in 0..=s.len() {
            let _ = truncate_utf8(s, max);
        }
        assert_eq!(truncate_utf8(s, 3), "你");
        assert_eq!(truncate_utf8(s, 6), "你好");
    }
}
