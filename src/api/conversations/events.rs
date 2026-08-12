//! Process-local conversation Server-Sent Events.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::{
    Json,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::sse::{Event, Sse},
    response::{IntoResponse, Response},
};
use tokio::sync::broadcast;

use super::{conversation_store_error, flow_segment, map_err_to_response};
use crate::api::{AppState, ErrorResponse, error_response, resolve_flow_path};
use crate::engine::eventbus::CrewEvent;
use crate::engine::sessions::validate_session_id;
use crate::engine::store::ConversationCoordinationScope;

fn event_type_str(event: &CrewEvent) -> &'static str {
    match event {
        CrewEvent::ConversationStarted { .. } => "conversation_started",
        CrewEvent::ConversationTurn { .. } => "conversation_turn",
        CrewEvent::ConversationThinking { .. } => "conversation_thinking",
        // Sub-crew lifecycle events — surfaced so the client can render
        // progress while a tool is delegating to a sub-flow via
        // `run_flow`. The chat transcript itself still only renders
        // `conversation_turn`; these go to the event stream panel.
        CrewEvent::CrewStarted { .. } => "crew_started",
        CrewEvent::PhaseStart { .. } => "phase_start",
        CrewEvent::TaskAssigned { .. } => "task_assigned",
        CrewEvent::TaskCompleted { .. } => "task_completed",
        CrewEvent::TaskFailed { .. } => "task_failed",
        CrewEvent::TaskThinking { .. } => "task_thinking",
        CrewEvent::ToolCall { .. } => "tool_call",
        CrewEvent::ToolResult { .. } => "tool_result",
        CrewEvent::AgentToolStarted { .. } => "agent_tool_started",
        CrewEvent::AgentToolCompleted { .. } => "agent_tool_completed",
        _ => "log",
    }
}

pub async fn conversation_events(
    State(state): State<Arc<AppState>>,
    Path((flow, id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Response, (StatusCode, Json<ErrorResponse>)> {
    let flow_path_resolved =
        resolve_flow_path(&state, &flow).map_err(|e| map_err_to_response(&e))?;
    validate_session_id(&id).map_err(|e| map_err_to_response(&e))?;

    let key = (flow_segment(&flow_path_resolved), id.clone());
    if state.store.conversation_coordination_scope() == ConversationCoordinationScope::SharedStore {
        let exists = state
            .store
            .get_conversation(Some(&key.0), &id)
            .await
            .map_err(conversation_store_error)?
            .is_some();
        if !exists {
            return Err(error_response(
                StatusCode::NOT_FOUND,
                format!("Conversation '{id}' not found"),
            ));
        }
        return Err(error_response(
            StatusCode::CONFLICT,
            "Conversation SSE replay is unavailable with shared-store coordination; use durable history for recovery"
                .into(),
        ));
    }
    if headers
        .get(axum::http::HeaderName::from_static("last-event-id"))
        .is_some()
    {
        return Err(error_response(
            StatusCode::CONFLICT,
            "Conversation SSE is process-local and does not support Last-Event-ID replay; reconnect without a cursor and recover durable messages from history"
                .into(),
        ));
    }
    let sse_permit = state.sse_permits.clone().try_acquire_owned().map_err(|_| {
        crate::metrics::record_sse(
            crate::metrics::SseScope::ConversationProcess,
            crate::metrics::SseOutcome::Limited,
        );
        error_response(
            StatusCode::TOO_MANY_REQUESTS,
            format!(
                "SSE connection limit reached ({})",
                state.max_sse_connections
            ),
        )
    })?;
    let handle = {
        let map = state.active_conversations.read().await;
        map.get(&key).cloned().ok_or_else(|| {
            error_response(
                StatusCode::NOT_FOUND,
                format!("Conversation '{}' is not active", id),
            )
        })?
    };

    let (replay, mut rx) = handle.eventbus.subscribe_with_replay();

    let stream = async_stream::stream! {
        let _sse_permit = sse_permit;
        for event in replay {
            if !is_conversation_event(&event) {
                continue;
            }
            let event_type = event_type_str(&event);
            let data = serde_json::to_string(&*event).unwrap_or_default();
            yield Ok::<Event, Infallible>(Event::default().event(event_type).data(data));
        }
        loop {
            match rx.recv().await {
                Ok(event) => {
                    if !is_conversation_event(&event) {
                        continue;
                    }
                    let event_type = event_type_str(&event);
                    let data = serde_json::to_string(&*event).unwrap_or_default();
                    yield Ok::<Event, Infallible>(Event::default().event(event_type).data(data));
                }
                Err(broadcast::error::RecvError::Lagged(skipped)) => {
                    let data = serde_json::json!({
                        "error": "conversation_event_gap",
                        "skipped_events": skipped,
                        "recovery": "read durable conversation history",
                    });
                    yield Ok::<Event, Infallible>(
                        Event::default()
                            .event("conversation_gap")
                            .data(data.to_string())
                    );
                    break;
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    };

    // `keep_alive` emits a comment-only event every 15 s so intermediate
    // proxies (Bun, reverse proxies, browser buffering) don't treat an
    // idle conversation as a stalled connection and tear it down.
    let response = Sse::new(stream)
        .keep_alive(
            axum::response::sse::KeepAlive::new()
                .interval(Duration::from_secs(15))
                .text("keepalive"),
        )
        .into_response();
    crate::metrics::record_sse(
        crate::metrics::SseScope::ConversationProcess,
        crate::metrics::SseOutcome::Accepted,
    );
    Ok(crate::api::sse::hardened_response(response))
}

fn is_conversation_event(event: &CrewEvent) -> bool {
    matches!(
        event,
        CrewEvent::ConversationStarted { .. }
            | CrewEvent::ConversationTurn { .. }
            | CrewEvent::ConversationThinking { .. }
            // Sub-crew progress events — fired when a tool delegates to
            // a sub-flow via `run_flow`. Surfaced so the UI can show
            // per-task progress during the turn instead of looking
            // frozen for 20-30 s.
            | CrewEvent::CrewStarted { .. }
            | CrewEvent::PhaseStart { .. }
            | CrewEvent::TaskAssigned { .. }
            | CrewEvent::TaskCompleted { .. }
            | CrewEvent::TaskFailed { .. }
            | CrewEvent::TaskThinking { .. }
            | CrewEvent::ToolCall { .. }
            | CrewEvent::ToolResult { .. }
            | CrewEvent::AgentToolStarted { .. }
            | CrewEvent::AgentToolCompleted { .. }
    )
}
