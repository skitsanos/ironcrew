//! Phase-1 Human-in-the-Loop: HTTP conversation endpoints.
//!
//! Wraps `crew:conversation({...})` behind six endpoints under
//! `/flows/{flow}/conversations`. Sessions are serialized per-id via a
//! `tokio::sync::Mutex<()>` on the `ConversationHandle` — concurrent
//! `POST /messages` calls for the same session queue rather than reject.
//!
//! Session creation is explicit: `POST /start` builds the session and
//! stashes it in `AppState.active_conversations`. `POST /messages` against
//! an unknown id returns 404 (never auto-creates).

use std::convert::Infallible;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
    response::sse::{Event, Sse},
};
use mlua::AnyUserData;
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, RwLock, broadcast};

use super::{AppState, ErrorResponse, error_response, resolve_flow_path};
use crate::engine::eventbus::{CrewEvent, EventBus};
use crate::engine::sessions::validate_session_id;
use crate::lua::api::{CHAT_CREW_REGISTRY_KEY, ChatMode, set_ironcrew_mode};
use crate::lua::conversation::{LuaConversation, LuaConversationInner};
use crate::utils::error::IronCrewError;

// ---------------------------------------------------------------------------
// Session handle
// ---------------------------------------------------------------------------

/// A live chat session. Holds the Lua VM (so the registered tools and
/// userdata stay alive), the shared conversation inner state, a per-session
/// event bus, and a mutex that serializes `run_turn` calls.
pub struct ConversationHandle {
    /// The Lua VM backing the session. Held in an `Arc` so the handle itself
    /// is `Send + Sync`. We never access `_lua` from multiple threads
    /// concurrently (turn_lock serializes all VM use), we just keep it alive.
    _lua: Arc<std::sync::Mutex<Option<mlua::Lua>>>,
    pub conv: Arc<LuaConversationInner>,
    pub eventbus: EventBus,
    pub turn_lock: Mutex<()>,
    #[allow(dead_code)]
    pub flow_path: String,
    pub id: String,
    pub agent: String,
    pub created_at: String,
    pub last_touched: RwLock<Instant>,
}

// ---------------------------------------------------------------------------
// Environment helpers
// ---------------------------------------------------------------------------

/// Default page size for `GET /flows/{flow}/conversations`.
fn conversations_default_limit() -> usize {
    std::env::var("IRONCREW_CONVERSATIONS_DEFAULT_LIMIT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(20)
}

/// Hard cap on page size.
fn conversations_max_limit() -> usize {
    std::env::var("IRONCREW_CONVERSATIONS_MAX_LIMIT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(100)
}

/// Idle timeout after which a session handle is evicted from memory. The
/// underlying record is kept in the store.
pub fn chat_session_idle_secs() -> u64 {
    std::env::var("IRONCREW_CHAT_SESSION_IDLE_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1800)
}

/// Cap on the number of simultaneously-active chat sessions.
pub fn max_active_conversations() -> usize {
    std::env::var("IRONCREW_MAX_ACTIVE_CONVERSATIONS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(100)
}

// ---------------------------------------------------------------------------
// Request / response types
// ---------------------------------------------------------------------------

#[derive(Deserialize, Default)]
pub struct StartReq {
    /// Agent name. Required when opening a new session; optional when
    /// re-starting an already active session (the existing handle's
    /// agent is reused).
    pub agent: Option<String>,
    pub max_history: Option<usize>,
}

#[derive(Serialize)]
pub struct StartResp {
    pub conversation_id: String,
    pub flow: String,
    pub agent: String,
    pub created_at: String,
    pub turn_count: usize,
    pub events_url: String,
}

#[derive(Deserialize, Default)]
pub struct MessageReq {
    pub content: String,
    #[serde(default)]
    pub images: Option<Vec<String>>,
}

#[derive(Serialize)]
pub struct MessageResp {
    pub conversation_id: String,
    pub turn_index: usize,
    pub assistant: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
    pub turn_count: usize,
}

#[derive(Serialize)]
pub struct HistoryResp {
    pub conversation_id: String,
    pub flow: Option<String>,
    pub agent: String,
    pub created_at: String,
    pub updated_at: String,
    pub messages: Vec<HistoryMessage>,
    pub turn_count: usize,
}

#[derive(Serialize)]
pub struct HistoryMessage {
    pub role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

#[derive(Deserialize)]
pub struct ListConversationsQuery {
    pub limit: Option<usize>,
    pub offset: Option<usize>,
}

#[derive(Serialize)]
pub struct ListConversationsResp {
    pub conversations: Vec<ConversationEntry>,
    pub total: u64,
    pub limit: usize,
    pub offset: usize,
}

#[derive(Serialize)]
pub struct ConversationEntry {
    pub id: String,
    pub flow: Option<String>,
    pub agent: String,
    pub created_at: String,
    pub updated_at: String,
    pub turn_count: usize,
    /// `true` when there is a live in-memory handle for this session.
    pub active: bool,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn map_err_to_response(e: &IronCrewError) -> (StatusCode, Json<ErrorResponse>) {
    let status = match e {
        IronCrewError::Validation(_) => StatusCode::BAD_REQUEST,
        IronCrewError::Io(_) => StatusCode::INTERNAL_SERVER_ERROR,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    error_response(status, e.to_string())
}

fn flow_segment(path: &std::path::Path) -> String {
    path.file_name()
        .and_then(|s| s.to_str())
        .map(|s| s.to_string())
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// POST /flows/{flow}/conversations/{id}/start
// ---------------------------------------------------------------------------

pub async fn start_conversation(
    State(state): State<Arc<AppState>>,
    Path((flow, id)): Path<(String, String)>,
    body: Option<Json<StartReq>>,
) -> Result<Json<StartResp>, (StatusCode, Json<ErrorResponse>)> {
    let req = body.map(|Json(b)| b).unwrap_or_default();

    let flow_path_resolved =
        resolve_flow_path(&state, &flow).map_err(|e| map_err_to_response(&e))?;
    validate_session_id(&id).map_err(|e| map_err_to_response(&e))?;

    let flow_slug = flow_segment(&flow_path_resolved);
    let key = (flow_slug.clone(), id.clone());

    // Idempotent: if a handle exists, return its current metadata. This
    // path does NOT require `agent` in the body — clients can restart a
    // session with `{}` and trust the server's stored agent.
    {
        let map = state.active_conversations.read().await;
        if let Some(existing) = map.get(&key) {
            let turn_count = existing.conv.turn_count().await;
            return Ok(Json(StartResp {
                conversation_id: existing.id.clone(),
                flow: flow.clone(),
                agent: existing.agent.clone(),
                created_at: existing.created_at.clone(),
                turn_count,
                events_url: format!("/flows/{}/conversations/{}/events", flow, id),
            }));
        }
    }

    // No active handle — decide whether this is a resume (store has a
    // prior record for this flow+id) or a fresh start. Resuming lets the
    // client re-activate an evicted or restarted session by posting
    // `{}` without re-sending the agent.
    let resume_agent: Option<String> = state
        .store
        .get_conversation(Some(&flow_slug), &id)
        .await
        .map_err(|e| map_err_to_response(&e))?
        .map(|r| r.agent_name);

    let agent_name = match (req.agent.as_deref().map(str::trim), resume_agent) {
        (Some(s), _) if !s.is_empty() => s.to_string(),
        (_, Some(stored)) => stored,
        _ => {
            return Err(error_response(
                StatusCode::BAD_REQUEST,
                "start: `agent` is required for a new conversation".into(),
            ));
        }
    };
    let req = StartReq {
        agent: Some(agent_name),
        max_history: req.max_history,
    };

    // Pre-check the cap BEFORE building the session — rejected starts must
    // not create observable side effects (no Lua VM spin-up, no persisted
    // bootstrap). The final cap re-check below closes the narrow TOCTOU
    // window where another request might fill the slot between here and
    // the insert.
    {
        let map = state.active_conversations.read().await;
        if !map.contains_key(&key) && map.len() >= state.max_active_conversations {
            return Err(error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                format!(
                    "Active conversation limit reached ({} sessions). Raise IRONCREW_MAX_ACTIVE_CONVERSATIONS or wait for idle eviction.",
                    state.max_active_conversations
                ),
            ));
        }
    }

    // Build a fresh session.
    let (handle, created_at, turn_count) =
        build_session(&state, &flow_path_resolved, &flow_slug, &id, &req).await?;
    let events_url = format!("/flows/{}/conversations/{}/events", flow, id);
    let agent = handle.agent.clone();

    // Insert under write-lock with a final cap re-check (TOCTOU guard).
    // If another request filled the last slot between the pre-check and
    // here, reject — and critically, do NOT persist the handle's bootstrap.
    {
        let mut map = state.active_conversations.write().await;
        if !map.contains_key(&key) && map.len() >= state.max_active_conversations {
            return Err(error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                format!(
                    "Active conversation limit reached ({} sessions). Raise IRONCREW_MAX_ACTIVE_CONVERSATIONS or wait for idle eviction.",
                    state.max_active_conversations
                ),
            ));
        }
        map.entry(key).or_insert_with(|| handle.clone());
    }

    // Only now that the handle is successfully in the active map do we
    // persist the bootstrap record. Rejected starts leave no trace in
    // the store or in `/conversations` listings.
    if let Err(e) = handle.conv.persist().await {
        tracing::warn!(
            conversation_id = %id,
            error = %e,
            "Failed to persist bootstrap conversation record on start"
        );
    }

    Ok(Json(StartResp {
        conversation_id: id,
        flow,
        agent,
        created_at,
        turn_count,
        events_url,
    }))
}

/// Helper — build the Lua VM + conversation inner, wrap in a
/// `ConversationHandle`, and return.
async fn build_session(
    state: &Arc<AppState>,
    flow_path: &std::path::Path,
    flow_slug: &str,
    id: &str,
    req: &StartReq,
) -> Result<(Arc<ConversationHandle>, String, usize), (StatusCode, Json<ErrorResponse>)> {
    use crate::cli::project::{load_project, setup_crew_runtime};

    let loader = load_project(flow_path).map_err(|e| map_err_to_response(&e))?;
    let (lua, _runtime) = setup_crew_runtime(&loader).map_err(|e| map_err_to_response(&e))?;

    // Mark chat mode so the Crew constructor parks its userdata in the
    // registry AND so user code can guard `crew:run()` appropriately.
    lua.set_app_data(ChatMode);
    // Share the server-wide store with the Lua VM so the LuaCrew
    // constructor reuses it instead of calling `create_store()` again
    // (which would re-run Postgres bootstrap on every session start).
    lua.set_app_data(state.store.clone());
    set_ironcrew_mode(&lua, "chat").map_err(|e| map_err_to_response(&IronCrewError::Lua(e)))?;

    // Per-session event bus. Conversation turn events flow through this bus.
    let eventbus = EventBus::new(256);
    lua.set_app_data(eventbus.clone());

    // Execute the entrypoint so the user's `Crew.new(...)` runs.
    let entrypoint = loader.entrypoint().ok_or_else(|| {
        error_response(
            StatusCode::BAD_REQUEST,
            "No entrypoint found in flow".into(),
        )
    })?;
    let script = std::fs::read_to_string(entrypoint)
        .map_err(|e| map_err_to_response(&IronCrewError::Io(e)))?;

    lua.load(&script)
        .exec_async()
        .await
        .map_err(|e| map_err_to_response(&IronCrewError::Lua(e)))?;

    // Pull the Crew userdata from the registry and call `conversation`.
    let crew_ud: AnyUserData = lua
        .named_registry_value(CHAT_CREW_REGISTRY_KEY)
        .map_err(|_| {
            error_response(
                StatusCode::BAD_REQUEST,
                "Flow did not construct a Crew in chat mode".into(),
            )
        })?;

    let max_history_field = match req.max_history {
        Some(n) => format!("max_history = {},", n),
        None => String::new(),
    };
    let snippet = format!(
        r#"
            local crew = ...
            return crew:conversation({{
                agent = {agent},
                id = {id},
                {max_history_field}
                stream = false,
            }})
        "#,
        agent = crate::cli::chat_lua_literal(req.agent.as_deref().unwrap_or("")),
        id = crate::cli::chat_lua_literal(id),
    );

    let conv_ud: AnyUserData = lua
        .load(&snippet)
        .call_async::<AnyUserData>(crew_ud)
        .await
        .map_err(|e| map_err_to_response(&IronCrewError::Lua(e)))?;

    let conv: Arc<LuaConversationInner> = {
        let wrapper = conv_ud
            .borrow::<LuaConversation>()
            .map_err(|e| map_err_to_response(&IronCrewError::Lua(e)))?;
        wrapper.inner()
    };

    let created_at = conv.created_at.clone();
    let turn_count = conv.turn_count().await;
    let agent = conv.agent.name.clone();

    // Release the conv_ud borrow before moving `lua`.
    drop(conv_ud);

    let handle = Arc::new(ConversationHandle {
        _lua: Arc::new(std::sync::Mutex::new(Some(lua))),
        conv,
        eventbus,
        turn_lock: Mutex::new(()),
        flow_path: flow_slug.to_string(),
        id: id.to_string(),
        agent: agent.clone(),
        created_at: created_at.clone(),
        last_touched: RwLock::new(Instant::now()),
    });

    let _ = state; // silence unused when no-op
    Ok((handle, created_at, turn_count))
}

// ---------------------------------------------------------------------------
// POST /flows/{flow}/conversations/{id}/messages
// ---------------------------------------------------------------------------

pub async fn post_message(
    State(state): State<Arc<AppState>>,
    Path((flow, id)): Path<(String, String)>,
    Json(req): Json<MessageReq>,
) -> Result<Json<MessageResp>, (StatusCode, Json<ErrorResponse>)> {
    if req.content.trim().is_empty() {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            "`content` is required".into(),
        ));
    }

    let flow_path_resolved =
        resolve_flow_path(&state, &flow).map_err(|e| map_err_to_response(&e))?;
    validate_session_id(&id).map_err(|e| map_err_to_response(&e))?;

    let key = (flow_segment(&flow_path_resolved), id.clone());
    let handle = {
        let map = state.active_conversations.read().await;
        map.get(&key).cloned().ok_or_else(|| {
            error_response(
                StatusCode::NOT_FOUND,
                format!("Conversation '{}' is not active — call /start first", id),
            )
        })?
    };

    // Load images (if any) before acquiring the turn lock.
    let images: Option<Vec<crate::llm::provider::ImageInput>> = match req.images {
        Some(paths) if !paths.is_empty() => {
            let client = reqwest::Client::new();
            let mut loaded = Vec::new();
            for p in paths {
                let img = crate::llm::image::load_image(&p, &flow_path_resolved, &client)
                    .await
                    .map_err(|e| map_err_to_response(&e))?;
                loaded.push(img);
            }
            Some(loaded)
        }
        _ => None,
    };

    let _guard = handle.turn_lock.lock().await;
    {
        let mut t = handle.last_touched.write().await;
        *t = Instant::now();
    }

    let (assistant, reasoning) = handle
        .conv
        .run_turn(&req.content, images)
        .await
        .map_err(|e| map_err_to_response(&e))?;

    let turn_count = handle.conv.turn_count().await;
    let turn_index = turn_count.saturating_sub(1);

    Ok(Json(MessageResp {
        conversation_id: id,
        turn_index,
        assistant,
        reasoning,
        turn_count,
    }))
}

// ---------------------------------------------------------------------------
// GET /flows/{flow}/conversations/{id}/history
// ---------------------------------------------------------------------------

pub async fn get_history(
    State(state): State<Arc<AppState>>,
    Path((flow, id)): Path<(String, String)>,
) -> Result<Json<HistoryResp>, (StatusCode, Json<ErrorResponse>)> {
    let flow_path_resolved =
        resolve_flow_path(&state, &flow).map_err(|e| map_err_to_response(&e))?;
    validate_session_id(&id).map_err(|e| map_err_to_response(&e))?;

    let flow_slug = flow_segment(&flow_path_resolved);
    let record = state
        .store
        .get_conversation(Some(&flow_slug), &id)
        .await
        .map_err(|e| map_err_to_response(&e))?
        .ok_or_else(|| {
            error_response(
                StatusCode::NOT_FOUND,
                format!("Conversation '{}' not found", id),
            )
        })?;

    let turn_count = record.messages.iter().filter(|m| m.role == "user").count();

    let messages: Vec<HistoryMessage> = record
        .messages
        .iter()
        .map(|m| HistoryMessage {
            role: m.role.clone(),
            content: m.content.clone(),
            tool_call_id: m.tool_call_id.clone(),
        })
        .collect();

    Ok(Json(HistoryResp {
        conversation_id: record.id,
        flow: record.flow_path,
        agent: record.agent_name,
        created_at: record.created_at,
        updated_at: record.updated_at,
        messages,
        turn_count,
    }))
}

// ---------------------------------------------------------------------------
// GET /flows/{flow}/conversations/{id}/events  (SSE)
// ---------------------------------------------------------------------------

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
        _ => "log",
    }
}

pub async fn conversation_events(
    State(state): State<Arc<AppState>>,
    Path((flow, id)): Path<(String, String)>,
) -> Result<
    Sse<impl futures::stream::Stream<Item = std::result::Result<Event, Infallible>>>,
    (StatusCode, Json<ErrorResponse>),
> {
    let flow_path_resolved =
        resolve_flow_path(&state, &flow).map_err(|e| map_err_to_response(&e))?;
    validate_session_id(&id).map_err(|e| map_err_to_response(&e))?;

    let key = (flow_segment(&flow_path_resolved), id.clone());
    let handle = {
        let map = state.active_conversations.read().await;
        map.get(&key).cloned().ok_or_else(|| {
            error_response(
                StatusCode::NOT_FOUND,
                format!("Conversation '{}' is not active", id),
            )
        })?
    };

    let replay = handle.eventbus.replay().await;
    let mut rx = handle.eventbus.subscribe();

    let stream = async_stream::stream! {
        for event in replay {
            if !is_conversation_event(&event) {
                continue;
            }
            let event_type = event_type_str(&event);
            let data = serde_json::to_string(&*event).unwrap_or_default();
            yield Ok(Event::default().event(event_type).data(data));
        }
        loop {
            match rx.recv().await {
                Ok(event) => {
                    if !is_conversation_event(&event) {
                        continue;
                    }
                    let event_type = event_type_str(&event);
                    let data = serde_json::to_string(&*event).unwrap_or_default();
                    yield Ok(Event::default().event(event_type).data(data));
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    // keep going
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    };

    // `keep_alive` emits a comment-only event every 15 s so intermediate
    // proxies (Bun, reverse proxies, browser buffering) don't treat an
    // idle conversation as a stalled connection and tear it down.
    Ok(Sse::new(stream).keep_alive(
        axum::response::sse::KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("keepalive"),
    ))
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
    )
}

// ---------------------------------------------------------------------------
// DELETE /flows/{flow}/conversations/{id}
// ---------------------------------------------------------------------------

pub async fn delete_conversation(
    State(state): State<Arc<AppState>>,
    Path((flow, id)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    let flow_path_resolved =
        resolve_flow_path(&state, &flow).map_err(|e| map_err_to_response(&e))?;
    validate_session_id(&id).map_err(|e| map_err_to_response(&e))?;

    let flow_slug = flow_segment(&flow_path_resolved);
    let key = (flow_slug.clone(), id.clone());

    // Drop the in-memory handle first.
    {
        let mut map = state.active_conversations.write().await;
        map.remove(&key);
    }

    // Then remove the persisted record, scoped to this flow so a delete
    // can never touch another flow's session with the same id.
    state
        .store
        .delete_conversation(Some(&flow_slug), &id)
        .await
        .map_err(|e| map_err_to_response(&e))?;

    Ok(Json(serde_json::json!({ "deleted": id })))
}

// ---------------------------------------------------------------------------
// GET /flows/{flow}/conversations
// ---------------------------------------------------------------------------

pub async fn list_conversations(
    State(state): State<Arc<AppState>>,
    Path(flow): Path<String>,
    Query(params): Query<ListConversationsQuery>,
) -> Result<Json<ListConversationsResp>, (StatusCode, Json<ErrorResponse>)> {
    let flow_path_resolved =
        resolve_flow_path(&state, &flow).map_err(|e| map_err_to_response(&e))?;

    let flow_slug = flow_segment(&flow_path_resolved);

    let default_limit = conversations_default_limit();
    let max_limit = conversations_max_limit();
    let limit = params.limit.unwrap_or(default_limit).min(max_limit).max(1);
    let offset = params.offset.unwrap_or(0);

    let summaries = state
        .store
        .list_conversations(Some(&flow_slug), limit, offset)
        .await
        .map_err(|e| map_err_to_response(&e))?;
    let total = state
        .store
        .count_conversations(Some(&flow_slug))
        .await
        .map_err(|e| map_err_to_response(&e))?;

    // Mark active sessions.
    let active_keys: std::collections::HashSet<String> = {
        let map = state.active_conversations.read().await;
        map.keys()
            .filter(|(fp, _)| fp == &flow_slug)
            .map(|(_, id)| id.clone())
            .collect()
    };

    let conversations: Vec<ConversationEntry> = summaries
        .into_iter()
        .map(|s| ConversationEntry {
            active: active_keys.contains(&s.id),
            id: s.id,
            flow: s.flow_path,
            agent: s.agent_name,
            created_at: s.created_at,
            updated_at: s.updated_at,
            turn_count: s.turn_count,
        })
        .collect();

    Ok(Json(ListConversationsResp {
        conversations,
        total,
        limit,
        offset,
    }))
}

// ---------------------------------------------------------------------------
// Idle eviction background task
// ---------------------------------------------------------------------------

/// Periodic eviction: every 60 seconds, drop handles whose `last_touched`
/// is older than `IRONCREW_CHAT_SESSION_IDLE_SECS`. Two-phase so we never
/// hold the write lock across an await.
pub async fn idle_eviction_loop(state: Arc<AppState>) {
    let sleep = Duration::from_secs(60);
    loop {
        tokio::time::sleep(sleep).await;
        let idle_cutoff = Duration::from_secs(chat_session_idle_secs());
        let now = Instant::now();

        // Phase 1 — collect expired keys under read lock.
        let expired: Vec<(String, String)> = {
            let map = state.active_conversations.read().await;
            let mut out = Vec::new();
            for (key, handle) in map.iter() {
                let last = *handle.last_touched.read().await;
                if now.duration_since(last) >= idle_cutoff {
                    out.push(key.clone());
                }
            }
            out
        };

        if expired.is_empty() {
            continue;
        }

        // Phase 2 — evict under write lock.
        {
            let mut map = state.active_conversations.write().await;
            for key in &expired {
                if let Some(handle) = map.remove(key) {
                    // Best-effort final persist — drop the final lock via
                    // spawn so we don't block the eviction scan.
                    let conv = handle.conv.clone();
                    tokio::spawn(async move {
                        let _ = conv.persist().await;
                    });
                }
            }
        }
        tracing::info!(
            evicted = expired.len(),
            "Evicted idle chat conversation handles"
        );
    }
}
