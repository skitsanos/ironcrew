//! Phase-1 Human-in-the-Loop: HTTP conversation endpoints.
//!
//! Wraps `crew:conversation({...})` behind six endpoints under
//! `/flows/{flow}/conversations`. Sessions are serialized per-id via a
//! `tokio::sync::Mutex<()>` on the `ConversationHandle`.
//!
//! Session creation is explicit: `POST /start` builds the session and
//! stashes it in `AppState.active_conversations`. `POST /messages` against
//! an unknown id returns 404 (never auto-creates). Overlapping mutations for
//! the same session fail fast instead of retaining an unbounded request queue.

use std::collections::HashMap;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex as StdMutex, OnceLock, Weak};
use std::time::{Duration, Instant};

use axum::{
    Json,
    extract::{ConnectInfo, Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::sse::{Event, Sse},
};
use mlua::AnyUserData;
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, OwnedMutexGuard, OwnedSemaphorePermit, RwLock, broadcast};

use super::{AppState, ErrorResponse, error_response, resolve_flow_path};
use crate::engine::eventbus::{CrewEvent, EventBus};
use crate::engine::idempotency::{
    CONVERSATION_MESSAGE_OPERATION, ConversationIdempotencyCommit, IdempotencyClaim,
    IdempotencyClaimOutcome, IdempotencyCompletion, IdempotencyLookup, IdempotencyRecord,
};
use crate::engine::sessions::validate_session_id;
use crate::lua::api::{CHAT_CREW_REGISTRY_KEY, ChatMode, set_ironcrew_mode};
use crate::lua::conversation::{LuaConversation, LuaConversationInner};
use crate::tools::ToolCallContext;
use crate::utils::error::IronCrewError;

type MessageResult = Result<(HeaderMap, Json<MessageResp>), (StatusCode, Json<ErrorResponse>)>;

#[derive(Clone)]
struct MessageIdempotencyAttempt {
    key_hash: String,
    request_fingerprint: String,
    attempt_id: String,
}

fn replay_message(record: &IdempotencyRecord) -> MessageResult {
    let (Some(status), Some(body)) = (record.response_status, record.response_body.as_deref())
    else {
        return Err(error_response(
            StatusCode::CONFLICT,
            "The prior message cannot be replayed; use a new Idempotency-Key after verifying the conversation history"
                .into(),
        ));
    };
    if status != StatusCode::OK.as_u16() {
        return Err(error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Stored message idempotency response has an invalid status".into(),
        ));
    }
    let response = serde_json::from_str::<MessageResp>(body).map_err(|error| {
        tracing::error!(%error, "Stored message idempotency response is corrupt");
        error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Stored idempotency response is corrupt".into(),
        )
    })?;
    Ok((super::idempotency::replay_headers(), Json(response)))
}

fn message_idempotency_store_error(error: IronCrewError) -> (StatusCode, Json<ErrorResponse>) {
    tracing::error!(%error, "Conversation idempotency storage operation failed");
    error_response(
        StatusCode::SERVICE_UNAVAILABLE,
        "Idempotency storage is temporarily unavailable".into(),
    )
}

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
    pub turn_lock: Arc<Mutex<()>>,
    /// Shutdown cancellation for an in-flight provider/tool turn. Dropping the
    /// selected `run_turn` future invokes its rollback guards before the pod
    /// releases this handle.
    pub shutdown: tokio::sync::watch::Sender<bool>,
    #[allow(dead_code)]
    pub flow_path: String,
    pub id: String,
    pub agent: String,
    pub created_at: String,
    pub last_touched: RwLock<Instant>,
    /// Keeps one server-wide conversation admission slot occupied for the
    /// full lifetime of this in-memory handle.
    _admission_permit: OwnedSemaphorePermit,
}

// ---------------------------------------------------------------------------
// Environment helpers
// ---------------------------------------------------------------------------

/// Default page size for `GET /flows/{flow}/conversations`.
fn conversations_default_limit() -> usize {
    positive_bounded_env(
        "IRONCREW_CONVERSATIONS_DEFAULT_LIMIT",
        20,
        conversations_max_limit(),
    )
}

/// Hard cap on page size.
fn conversations_max_limit() -> usize {
    positive_bounded_env("IRONCREW_CONVERSATIONS_MAX_LIMIT", 100, 1000)
}

/// Idle timeout after which a session handle is evicted from memory. The
/// underlying record is kept in the store.
pub fn chat_session_idle_secs() -> u64 {
    positive_bounded_env("IRONCREW_CHAT_SESSION_IDLE_SECS", 1800, 7 * 24 * 60 * 60) as u64
}

/// Cap on the number of simultaneously-active chat sessions.
pub fn max_active_conversations() -> usize {
    positive_bounded_env("IRONCREW_MAX_ACTIVE_CONVERSATIONS", 8, 1024)
}

fn max_conversation_turn_secs() -> u64 {
    positive_bounded_env("IRONCREW_MAX_CONVERSATION_TURN_SECS", 300, 3600) as u64
}

fn positive_bounded_env(name: &str, default: usize, upper: usize) -> usize {
    let fallback = default.min(upper);
    match std::env::var(name) {
        Ok(raw) => match raw.parse::<usize>() {
            Ok(value) if value > 0 => value.min(upper),
            _ => {
                tracing::warn!(
                    variable = name,
                    value = %raw,
                    default = fallback,
                    "Ignoring invalid resource-limit environment value"
                );
                fallback
            }
        },
        Err(_) => fallback,
    }
}

fn api_max_history() -> usize {
    positive_bounded_env("IRONCREW_API_CONVERSATION_MAX_HISTORY", 50, 1000)
}

fn api_message_max_bytes() -> usize {
    positive_bounded_env(
        "IRONCREW_API_MESSAGE_MAX_BYTES",
        256 * 1024,
        4 * 1024 * 1024,
    )
}

fn api_max_images_per_message() -> usize {
    positive_bounded_env("IRONCREW_API_MAX_IMAGES_PER_MESSAGE", 4, 32)
}

fn api_max_images_per_conversation() -> usize {
    positive_bounded_env("IRONCREW_API_MAX_IMAGES_PER_CONVERSATION", 16, 256)
}

fn api_max_image_bytes_per_message() -> usize {
    positive_bounded_env(
        "IRONCREW_API_MAX_IMAGE_BYTES_PER_MESSAGE",
        20 * 1024 * 1024,
        100 * 1024 * 1024,
    )
}

fn api_max_image_bytes_per_conversation() -> usize {
    positive_bounded_env(
        "IRONCREW_API_MAX_IMAGE_BYTES_PER_CONVERSATION",
        32 * 1024 * 1024,
        512 * 1024 * 1024,
    )
}

fn api_max_image_locator_bytes() -> usize {
    positive_bounded_env("IRONCREW_API_MAX_IMAGE_LOCATOR_BYTES", 2048, 16 * 1024)
}

type ConversationKey = (String, String);
type LifecycleLock = Mutex<()>;

/// Per-conversation operation gates. Weak entries keep the registry bounded:
/// once no request owns a gate, the next lookup prunes it. Serializing start,
/// message, delete, and eviction gives each operation a clear linearization
/// point without holding the global active-conversation map across LLM calls.
static CONVERSATION_LIFECYCLES: OnceLock<StdMutex<HashMap<ConversationKey, Weak<LifecycleLock>>>> =
    OnceLock::new();

fn lifecycle_lock(key: &ConversationKey) -> Arc<LifecycleLock> {
    let registry = CONVERSATION_LIFECYCLES.get_or_init(|| StdMutex::new(HashMap::new()));
    let mut registry = registry
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    registry.retain(|_, lock| lock.strong_count() > 0);
    if let Some(lock) = registry.get(key).and_then(Weak::upgrade) {
        return lock;
    }
    let lock = Arc::new(Mutex::new(()));
    registry.insert(key.clone(), Arc::downgrade(&lock));
    lock
}

fn decoded_base64_len(data: &str) -> usize {
    let padding = data
        .as_bytes()
        .iter()
        .rev()
        .take_while(|byte| **byte == b'=')
        .count();
    data.len()
        .saturating_div(4)
        .saturating_mul(3)
        .saturating_sub(padding)
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

#[derive(Clone, Deserialize, Default)]
pub struct MessageReq {
    pub content: String,
    #[serde(default)]
    pub images: Option<Vec<String>>,
}

#[derive(Clone, Deserialize, Serialize)]
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
    pub truncated: bool,
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
        IronCrewError::Conflict(_) => StatusCode::CONFLICT,
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
    headers: HeaderMap,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    body: Option<Json<StartReq>>,
) -> Result<Json<StartResp>, (StatusCode, Json<ErrorResponse>)> {
    let req = body.map(|Json(b)| b).unwrap_or_default();
    let agent_for_audit = req.agent.clone();

    let result = start_conversation_inner(state.clone(), flow.clone(), id.clone(), req).await;

    let (success, status_code) = match &result {
        Ok(_) => (true, 200u16),
        Err((sc, _)) => (false, sc.as_u16()),
    };
    let metadata = agent_for_audit.map(|a| serde_json::json!({ "agent": a }));

    crate::api::audit::record(
        &state.store,
        "conversation.start",
        Some(&flow),
        Some(&id),
        &headers,
        Some(addr),
        success,
        status_code,
        metadata,
    )
    .await;

    result
}

async fn start_conversation_inner(
    state: Arc<AppState>,
    flow: String,
    id: String,
    req: StartReq,
) -> Result<Json<StartResp>, (StatusCode, Json<ErrorResponse>)> {
    let flow_path_resolved =
        resolve_flow_path(&state, &flow).map_err(|e| map_err_to_response(&e))?;
    validate_session_id(&id).map_err(|e| map_err_to_response(&e))?;

    if !state
        .accepting_traffic
        .load(std::sync::atomic::Ordering::Acquire)
    {
        return Err(error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "Server is shutting down".into(),
        ));
    }

    let flow_slug = flow_segment(&flow_path_resolved);
    let key = (flow_slug.clone(), id.clone());
    let lifecycle = lifecycle_lock(&key);
    let _lifecycle_guard = lifecycle.try_lock().map_err(|_| {
        error_response(
            StatusCode::CONFLICT,
            "Conversation is busy; retry after the active operation completes".into(),
        )
    })?;

    let history_cap = api_max_history();
    if req
        .max_history
        .is_some_and(|value| value == 0 || value > history_cap)
    {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            format!("`max_history` must be between 1 and {history_cap} for HTTP conversations"),
        ));
    }

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
        max_history: Some(req.max_history.unwrap_or(history_cap)),
    };

    // Reserve a slot atomically before building the Lua VM. A plain
    // `map.len()` check races concurrent starts; an owned semaphore permit
    // cannot be oversubscribed and is released automatically with the handle.
    let admission_permit = state
        .conversation_permits
        .clone()
        .try_acquire_owned()
        .map_err(|_| {
            error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                format!(
                    "Active conversation limit reached ({} sessions). Raise IRONCREW_MAX_ACTIVE_CONVERSATIONS or wait for idle eviction.",
                    state.max_active_conversations
                ),
            )
        })?;

    // Build a fresh session.
    let (handle, created_at, turn_count) = build_session(
        &state,
        &flow_path_resolved,
        &flow_slug,
        &id,
        &req,
        admission_permit,
    )
    .await?;
    let events_url = format!("/flows/{}/conversations/{}/events", flow, id);

    // Establish durability before publishing the handle. If this request is
    // cancelled during persistence, the candidate handle (and permit) drops;
    // a successfully-written record is safely resumable on the next start.
    handle.conv.persist().await.map_err(|error| {
        error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to persist conversation bootstrap: {error}"),
        )
    })?;

    // Resolve a same-id creation race under the map lock. Only the winning
    // handle is published and returned; the losing candidate is dropped,
    // which also returns its unused admission permit. The lifecycle lock
    // normally makes the occupied branch unreachable, but the map check keeps
    // this invariant robust to future callers.
    let (selected, inserted) = {
        use std::collections::hash_map::Entry;
        let mut map = state.active_conversations.write().await;
        if !state
            .accepting_traffic
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return Err(error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                "Server is shutting down".into(),
            ));
        }
        match map.entry(key) {
            Entry::Vacant(entry) => {
                entry.insert(handle.clone());
                (handle.clone(), true)
            }
            Entry::Occupied(entry) => (entry.get().clone(), false),
        }
    };

    if !inserted {
        let selected_turn_count = selected.conv.turn_count().await;
        return Ok(Json(StartResp {
            conversation_id: selected.id.clone(),
            flow,
            agent: selected.agent.clone(),
            created_at: selected.created_at.clone(),
            turn_count: selected_turn_count,
            events_url,
        }));
    }

    Ok(Json(StartResp {
        conversation_id: id,
        flow,
        agent: selected.agent.clone(),
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
    admission_permit: OwnedSemaphorePermit,
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
    let script = crate::lua::source::read_lua_source(entrypoint)
        .map_err(|error| map_err_to_response(&error))?;

    {
        let _execution = crate::lua::limits::LuaExecutionGuard::begin(&lua)
            .map_err(|e| map_err_to_response(&IronCrewError::Lua(e)))?;
        lua.load(&script)
            .exec_async()
            .await
            .map_err(|e| map_err_to_response(&IronCrewError::Lua(e)))?;
    }

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

    let conv_ud: AnyUserData = {
        let _execution = crate::lua::limits::LuaExecutionGuard::begin(&lua)
            .map_err(|e| map_err_to_response(&IronCrewError::Lua(e)))?;
        lua.load(&snippet)
            .call_async::<AnyUserData>(crew_ud)
            .await
            .map_err(|e| map_err_to_response(&IronCrewError::Lua(e)))?
    };

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

    let (shutdown, _) = tokio::sync::watch::channel(false);
    let handle = Arc::new(ConversationHandle {
        _lua: Arc::new(std::sync::Mutex::new(Some(lua))),
        conv,
        eventbus,
        turn_lock: Arc::new(Mutex::new(())),
        shutdown,
        flow_path: flow_slug.to_string(),
        id: id.to_string(),
        agent: agent.clone(),
        created_at: created_at.clone(),
        last_touched: RwLock::new(Instant::now()),
        _admission_permit: admission_permit,
    });

    let _ = state; // silence unused when no-op
    Ok((handle, created_at, turn_count))
}

// ---------------------------------------------------------------------------
// POST /flows/{flow}/conversations/{id}/messages
// ---------------------------------------------------------------------------

async fn mark_message_indeterminate(state: &Arc<AppState>, attempt: &MessageIdempotencyAttempt) {
    let completed_at = chrono::Utc::now();
    let completed_at_text = completed_at.to_rfc3339();
    let expires_at = state.idempotency.retention_expiry(completed_at);
    let mut persistence_degraded = false;
    let mut retry_delay = Duration::from_millis(250);
    loop {
        match state
            .store
            .mark_idempotency_indeterminate(
                &attempt.key_hash,
                &attempt.attempt_id,
                &completed_at_text,
                &expires_at,
            )
            .await
        {
            Ok(updated) => {
                if !updated {
                    tracing::warn!(
                        "Conversation idempotency claim disappeared before indeterminate finalization"
                    );
                }
                if persistence_degraded {
                    state
                        .terminal_persistence_failures
                        .fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
                }
                return;
            }
            Err(error @ IronCrewError::Conflict(_)) => {
                tracing::warn!(%error, "Indeterminate conversation finalization was fenced");
                if persistence_degraded {
                    state
                        .terminal_persistence_failures
                        .fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
                }
                return;
            }
            Err(error) => {
                if !persistence_degraded {
                    persistence_degraded = true;
                    state
                        .terminal_persistence_failures
                        .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
                }
                tracing::error!(
                    %error,
                    retry_ms = retry_delay.as_millis(),
                    "Failed to preserve an indeterminate conversation outcome; retrying"
                );
                tokio::time::sleep(retry_delay).await;
                retry_delay = retry_delay.saturating_mul(2).min(Duration::from_secs(30));
            }
        }
    }
}

async fn commit_message_with_retry(
    state: &Arc<AppState>,
    completion: IdempotencyCompletion,
    conversation: &crate::engine::sessions::ConversationRecord,
) -> Result<ConversationIdempotencyCommit, IronCrewError> {
    let mut persistence_degraded = false;
    let mut retry_delay = Duration::from_millis(250);
    loop {
        match state
            .store
            .commit_conversation_idempotency(
                completion.clone(),
                conversation,
                state.idempotency.max_total_response_bytes,
            )
            .await
        {
            Ok(committed) => {
                if persistence_degraded {
                    state
                        .terminal_persistence_failures
                        .fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
                }
                return Ok(committed);
            }
            Err(error @ IronCrewError::Conflict(_)) => {
                if persistence_degraded {
                    state
                        .terminal_persistence_failures
                        .fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
                }
                return Err(error);
            }
            Err(error) => {
                if !persistence_degraded {
                    persistence_degraded = true;
                    state
                        .terminal_persistence_failures
                        .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
                }
                tracing::error!(
                    %error,
                    retry_ms = retry_delay.as_millis(),
                    "Atomic conversation/idempotency commit failed; retaining the turn and retrying"
                );
                tokio::time::sleep(retry_delay).await;
                retry_delay = retry_delay.saturating_mul(2).min(Duration::from_secs(30));
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn execute_idempotent_message(
    state: Arc<AppState>,
    key: ConversationKey,
    handle: Arc<ConversationHandle>,
    id: String,
    content: String,
    images: Option<Vec<crate::llm::provider::ImageInput>>,
    attempt: MessageIdempotencyAttempt,
    _lifecycle_guard: OwnedMutexGuard<()>,
    _turn_guard: OwnedMutexGuard<()>,
) -> Result<MessageResp, (StatusCode, Json<ErrorResponse>)> {
    let heartbeat = super::idempotency::LeaseHeartbeat::spawn(
        state.store.clone(),
        attempt.key_hash.clone(),
        attempt.attempt_id.clone(),
        CONVERSATION_MESSAGE_OPERATION,
    );
    let mut claim_loss = heartbeat.loss_receiver();
    let turn_timeout = Duration::from_secs(max_conversation_turn_secs());
    let mut shutdown = handle.shutdown.subscribe();
    let caller_context = ToolCallContext::default();
    let turn = tokio::time::timeout(
        turn_timeout,
        handle
            .conv
            .prepare_turn_with_ctx(&content, images, &caller_context),
    );
    enum TurnOutcome {
        Shutdown,
        ClaimLost,
        Finished(
            Box<
                std::result::Result<
                    Result<crate::lua::conversation::PreparedConversationTurn, IronCrewError>,
                    tokio::time::error::Elapsed,
                >,
            >,
        ),
    }
    let already_stopping = *shutdown.borrow();
    let outcome = if already_stopping {
        TurnOutcome::Shutdown
    } else {
        tokio::select! {
            biased;
            _ = shutdown.changed() => TurnOutcome::Shutdown,
            _ = super::idempotency::wait_for_lease_loss(&mut claim_loss) => TurnOutcome::ClaimLost,
            result = turn => TurnOutcome::Finished(Box::new(result)),
        }
    };
    let prepared = match outcome {
        TurnOutcome::Shutdown => {
            drop(heartbeat);
            mark_message_indeterminate(&state, &attempt).await;
            return Err(error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                "Conversation turn cancelled during server shutdown".into(),
            ));
        }
        TurnOutcome::ClaimLost => {
            drop(heartbeat);
            state.active_conversations.write().await.remove(&key);
            mark_message_indeterminate(&state, &attempt).await;
            return Err(error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                "Conversation turn stopped after its idempotency claim was lost; inspect history before retrying"
                    .into(),
            ));
        }
        TurnOutcome::Finished(result) => match *result {
            Err(_) => {
                drop(heartbeat);
                mark_message_indeterminate(&state, &attempt).await;
                return Err(error_response(
                    StatusCode::GATEWAY_TIMEOUT,
                    format!(
                        "Conversation turn exceeded IRONCREW_MAX_CONVERSATION_TURN_SECS ({})",
                        turn_timeout.as_secs()
                    ),
                ));
            }
            Ok(Err(error)) => {
                drop(heartbeat);
                mark_message_indeterminate(&state, &attempt).await;
                return Err(map_err_to_response(&error));
            }
            Ok(Ok(prepared)) => prepared,
        },
    };

    let response = MessageResp {
        conversation_id: id,
        turn_index: prepared.turn_index,
        assistant: prepared.assistant.clone(),
        reasoning: prepared.reasoning.clone(),
        turn_count: prepared.turn_count,
    };
    let response_body = match super::idempotency::bounded_response_json(
        &response,
        state.idempotency.max_response_bytes,
    ) {
        Ok(body) => body,
        Err(error) => {
            drop(heartbeat);
            mark_message_indeterminate(&state, &attempt).await;
            return Err(message_idempotency_store_error(error));
        }
    };
    let completed_at = chrono::Utc::now();
    let completion = IdempotencyCompletion {
        key_hash: attempt.key_hash.clone(),
        request_fingerprint: attempt.request_fingerprint.clone(),
        attempt_id: attempt.attempt_id.clone(),
        owner_instance_id: state.store.instance_id().to_string(),
        response_status: StatusCode::OK.as_u16(),
        response_body,
        completed_at: completed_at.to_rfc3339(),
        expires_at: state.idempotency.retention_expiry(completed_at),
    };

    let commit_result = {
        let commit = commit_message_with_retry(&state, completion, &prepared.record);
        tokio::pin!(commit);
        tokio::select! {
            biased;
            result = &mut commit => result,
            _ = super::idempotency::wait_for_lease_loss(&mut claim_loss) => {
                drop(heartbeat);
                state.active_conversations.write().await.remove(&key);
                mark_message_indeterminate(&state, &attempt).await;
                return Err(error_response(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Conversation commit stopped after its idempotency claim was lost; inspect history before retrying"
                        .into(),
                ));
            }
        }
    };
    let committed = match commit_result {
        Ok(committed) => committed,
        Err(error @ IronCrewError::Conflict(_)) => {
            drop(heartbeat);
            state.active_conversations.write().await.remove(&key);
            mark_message_indeterminate(&state, &attempt).await;
            return Err(error_response(StatusCode::CONFLICT, error.to_string()));
        }
        Err(error) => {
            drop(heartbeat);
            mark_message_indeterminate(&state, &attempt).await;
            return Err(message_idempotency_store_error(error));
        }
    };
    drop(heartbeat);

    if committed.already_completed {
        // A previous commit attempt reached durable storage but its reply was
        // lost. Another pod may have advanced the conversation since then;
        // never publish this older prepared snapshot into the live handle.
        state.active_conversations.write().await.remove(&key);
        return Ok(response);
    }

    if let Err(error) = handle
        .conv
        .publish_prepared_turn(prepared, committed.revision)
        .await
    {
        // The durable transcript and replay response are authoritative. Drop
        // a stale in-memory handle so the next /start reloads that revision.
        state.active_conversations.write().await.remove(&key);
        tracing::error!(%error, "Failed to publish a durably committed conversation turn");
        return Err(error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Conversation committed but the live session must be restarted".into(),
        ));
    }

    *handle.last_touched.write().await = Instant::now();
    Ok(response)
}

pub async fn post_message(
    State(state): State<Arc<AppState>>,
    Path((flow, id)): Path<(String, String)>,
    headers: HeaderMap,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Json(req): Json<MessageReq>,
) -> MessageResult {
    if !state
        .accepting_traffic
        .load(std::sync::atomic::Ordering::Acquire)
    {
        return Err(error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "Server is shutting down".into(),
        ));
    }
    if req.content.trim().is_empty() {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            "`content` is required".into(),
        ));
    }
    let message_max_bytes = api_message_max_bytes();
    if req.content.len() > message_max_bytes {
        return Err(error_response(
            StatusCode::PAYLOAD_TOO_LARGE,
            format!(
                "`content` is {} bytes, exceeds IRONCREW_API_MESSAGE_MAX_BYTES ({message_max_bytes})",
                req.content.len()
            ),
        ));
    }

    let flow_path_resolved =
        resolve_flow_path(&state, &flow).map_err(|e| map_err_to_response(&e))?;
    validate_session_id(&id).map_err(|e| map_err_to_response(&e))?;

    let request_key = super::idempotency::request_key(&headers, state.idempotency.require_key)
        .map_err(|error| error_response(StatusCode::BAD_REQUEST, error.to_string()))?;
    let recovery_key = super::idempotency::recovery_key(&headers)
        .map_err(|error| error_response(StatusCode::BAD_REQUEST, error.to_string()))?;
    if recovery_key.is_some() && request_key.is_none() {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            "Idempotency-Recovery-Key requires a new Idempotency-Key".into(),
        ));
    }
    if recovery_key.is_some()
        && recovery_key.as_ref().map(|key| &key.key_hash)
            == request_key.as_ref().map(|key| &key.key_hash)
    {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            "Idempotency-Recovery-Key must name a different prior key".into(),
        ));
    }
    let flow_slug = flow_segment(&flow_path_resolved);
    let request_fingerprint = super::idempotency::conversation_message_fingerprint(
        &flow_slug,
        &id,
        &req.content,
        req.images.as_deref(),
    );
    if let Some(request_key) = request_key.as_ref() {
        let now = chrono::Utc::now().to_rfc3339();
        match state
            .store
            .lookup_idempotency(&request_key.key_hash, &request_fingerprint, &now)
            .await
            .map_err(message_idempotency_store_error)?
        {
            IdempotencyLookup::Miss => {}
            IdempotencyLookup::Replay(record) => return replay_message(&record),
            IdempotencyLookup::InProgress(_) => {
                return Err(error_response(
                    StatusCode::CONFLICT,
                    "This idempotent message is already in progress; retry shortly".into(),
                ));
            }
            IdempotencyLookup::Indeterminate(_) => {
                return Err(error_response(
                    StatusCode::CONFLICT,
                    "The prior message has an indeterminate outcome; inspect history before using a new Idempotency-Key"
                        .into(),
                ));
            }
            IdempotencyLookup::Conflict => {
                return Err(error_response(
                    StatusCode::CONFLICT,
                    "Idempotency-Key was already used for a different request".into(),
                ));
            }
        }
    }

    let key = (flow_slug, id.clone());
    let lifecycle = lifecycle_lock(&key);
    let lifecycle_guard = lifecycle.clone().try_lock_owned().map_err(|_| {
        error_response(
            StatusCode::CONFLICT,
            "Conversation is busy; retry after the active operation completes".into(),
        )
    })?;
    let handle = {
        let map = state.active_conversations.read().await;
        map.get(&key).cloned().ok_or_else(|| {
            error_response(
                StatusCode::NOT_FOUND,
                format!("Conversation '{}' is not active — call /start first", id),
            )
        })?
    };

    // The lifecycle gate prevents delete/eviction/recreation from crossing
    // this turn. Fail fast if another path already owns the handle instead of
    // retaining parsed requests in an unbounded same-session queue.
    let turn_guard = handle.turn_lock.clone().try_lock_owned().map_err(|_| {
        error_response(
            StatusCode::CONFLICT,
            "Conversation is busy; retry after the active operation completes".into(),
        )
    })?;
    if !state
        .accepting_traffic
        .load(std::sync::atomic::Ordering::Acquire)
    {
        return Err(error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "Server is shutting down".into(),
        ));
    }
    let (history_image_count, history_image_bytes) = {
        let history = handle.conv.messages.lock().await;
        history
            .iter()
            .filter_map(|message| message.images.as_ref())
            .flatten()
            .fold((0usize, 0usize), |(count, bytes), image| {
                (
                    count.saturating_add(1),
                    bytes.saturating_add(decoded_base64_len(&image.data)),
                )
            })
    };

    let images: Option<Vec<crate::llm::provider::ImageInput>> = match req.images {
        Some(paths) if !paths.is_empty() => {
            let max_per_message = api_max_images_per_message();
            let max_per_conversation = api_max_images_per_conversation();
            if paths.len() > max_per_message
                || history_image_count.saturating_add(paths.len()) > max_per_conversation
            {
                return Err(error_response(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    format!(
                        "Image count exceeds limits ({max_per_message} per message, {max_per_conversation} per conversation)"
                    ),
                ));
            }
            let max_locator_bytes = api_max_image_locator_bytes();
            if let Some(path) = paths
                .iter()
                .find(|path| path.is_empty() || path.len() > max_locator_bytes)
            {
                return Err(error_response(
                    StatusCode::BAD_REQUEST,
                    format!(
                        "Image locator must be 1..={max_locator_bytes} bytes (received {})",
                        path.len()
                    ),
                ));
            }

            let max_conversation_bytes = api_max_image_bytes_per_conversation();
            let conversation_remaining = max_conversation_bytes.saturating_sub(history_image_bytes);
            let mut remaining = api_max_image_bytes_per_message().min(conversation_remaining);
            if remaining == 0 {
                return Err(error_response(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "Conversation image-byte limit reached".into(),
                ));
            }

            // Use the shared client so image-URL fetches inherit the SSRF
            // redirect policy (a fresh Client would follow redirects to
            // private addresses unchecked).
            let client = crate::tools::http_request::SHARED_HTTP_CLIENT.clone();
            let mut loaded = Vec::with_capacity(paths.len());
            for p in paths {
                let img = crate::llm::image::load_image_with_limit(
                    &p,
                    &flow_path_resolved,
                    &client,
                    remaining,
                )
                .await
                .map_err(|e| map_err_to_response(&e))?;
                let decoded_bytes = decoded_base64_len(&img.data);
                if decoded_bytes > remaining {
                    return Err(error_response(
                        StatusCode::PAYLOAD_TOO_LARGE,
                        "Aggregate image-byte limit exceeded".into(),
                    ));
                }
                remaining -= decoded_bytes;
                loaded.push(img);
            }
            Some(loaded)
        }
        _ => None,
    };

    if let Some(request_key) = request_key {
        let scope = super::idempotency::conversation_scope(&key.0, &id);
        let now = chrono::Utc::now();
        let attempt_id = uuid::Uuid::new_v4().to_string();
        let lease_expires_at = now
            .checked_add_signed(
                chrono::Duration::from_std(state.store.run_lease_ttl())
                    .unwrap_or_else(|_| chrono::Duration::seconds(60)),
            )
            .unwrap_or(chrono::DateTime::<chrono::Utc>::MAX_UTC)
            .to_rfc3339();
        let claim = IdempotencyClaim {
            key_hash: request_key.key_hash.clone(),
            recovery_key_hash: recovery_key.as_ref().map(|key| key.key_hash.clone()),
            request_fingerprint: request_fingerprint.clone(),
            operation: CONVERSATION_MESSAGE_OPERATION.into(),
            // Stores use `scope` as the durable conversation flow_path and
            // `exclusive_scope` as the full per-conversation mutation gate.
            scope: key.0.clone(),
            resource_id: id.clone(),
            exclusive_scope: Some(scope),
            attempt_id: attempt_id.clone(),
            owner_instance_id: state.store.instance_id().to_string(),
            base_revision: Some(handle.conv.revision().await),
            response_status: None,
            response_body: None,
            max_total_response_bytes: state.idempotency.max_total_response_bytes,
            lease_expires_at,
            created_at: now.to_rfc3339(),
            ttl_seconds: state.idempotency.ttl_seconds,
        };
        let attempt = MessageIdempotencyAttempt {
            key_hash: request_key.key_hash,
            request_fingerprint,
            attempt_id,
        };
        match state
            .store
            .claim_idempotency(
                claim,
                state.idempotency.max_records,
                state.idempotency.prune_batch,
            )
            .await
            .map_err(message_idempotency_store_error)?
        {
            IdempotencyClaimOutcome::Claimed(_) => {}
            IdempotencyClaimOutcome::Replay(record) => return replay_message(&record),
            IdempotencyClaimOutcome::InProgress(_) => {
                return Err(error_response(
                    StatusCode::CONFLICT,
                    "This idempotent message is already in progress; retry shortly".into(),
                ));
            }
            IdempotencyClaimOutcome::Indeterminate(_) => {
                return Err(error_response(
                    StatusCode::CONFLICT,
                    "The prior message has an indeterminate outcome; inspect history before using a new Idempotency-Key"
                        .into(),
                ));
            }
            IdempotencyClaimOutcome::Conflict => {
                // The key may have raced another pod, or the durable
                // conversation revision may be newer than this process-local
                // Lua handle. In either case, invalidate it while holding the
                // lifecycle gate so `/start` is forced to reload storage.
                state.active_conversations.write().await.remove(&key);
                return Err(error_response(
                    StatusCode::CONFLICT,
                    "Idempotency claim conflicted with durable state; reopen the conversation and retry with the appropriate key"
                        .into(),
                ));
            }
            IdempotencyClaimOutcome::Busy => {
                return Err(error_response(
                    StatusCode::CONFLICT,
                    "Conversation is busy or has an unacknowledged indeterminate turn; inspect history, then retry with Idempotency-Recovery-Key when required"
                        .into(),
                ));
            }
        }

        let task_state = state.clone();
        let audit_state = state.clone();
        let audit_flow = flow.clone();
        let audit_id = id.clone();
        let audit_headers = crate::api::audit::background_headers(&headers);
        let task = tokio::spawn(async move {
            let result = execute_idempotent_message(
                task_state,
                key,
                handle,
                id,
                req.content,
                images,
                attempt,
                lifecycle_guard,
                turn_guard,
            )
            .await;
            let (success, status_code, metadata) = match &result {
                Ok(response) => (
                    true,
                    StatusCode::OK.as_u16(),
                    Some(serde_json::json!({
                        "idempotent": true,
                        "turn_index": response.turn_index,
                        "turn_count": response.turn_count,
                    })),
                ),
                Err((status, _)) => (
                    false,
                    status.as_u16(),
                    Some(serde_json::json!({ "idempotent": true })),
                ),
            };
            crate::api::audit::record(
                &audit_state.store,
                "conversation.message",
                Some(&audit_flow),
                Some(&audit_id),
                &audit_headers,
                Some(addr),
                success,
                status_code,
                metadata,
            )
            .await;
            result
        });
        let response = task.await.map_err(|error| {
            tracing::error!(%error, "Idempotent conversation task panicked");
            error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Conversation task failed unexpectedly".into(),
            )
        })??;
        return Ok((HeaderMap::new(), Json(response)));
    }

    *handle.last_touched.write().await = Instant::now();

    let turn_timeout = Duration::from_secs(max_conversation_turn_secs());
    let mut shutdown = handle.shutdown.subscribe();
    let turn = tokio::time::timeout(turn_timeout, handle.conv.run_turn(&req.content, images));
    let turn_result = tokio::select! {
        biased;
        _ = shutdown.wait_for(|stopping| *stopping) => {
            return Err(error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                "Conversation turn cancelled during server shutdown".into(),
            ));
        }
        result = turn => result,
    };
    let (assistant, reasoning) = turn_result
        .map_err(|_| {
            error_response(
                StatusCode::GATEWAY_TIMEOUT,
                format!(
                    "Conversation turn exceeded IRONCREW_MAX_CONVERSATION_TURN_SECS ({})",
                    turn_timeout.as_secs()
                ),
            )
        })?
        .map_err(|e| map_err_to_response(&e))?;

    let turn_count = handle.conv.turn_count().await;
    let turn_index = turn_count.saturating_sub(1);

    crate::api::audit::record(
        &state.store,
        "conversation.message",
        Some(&flow),
        Some(&id),
        &headers,
        Some(addr),
        true,
        StatusCode::OK.as_u16(),
        Some(serde_json::json!({
            "idempotent": false,
            "turn_index": turn_index,
            "turn_count": turn_count,
        })),
    )
    .await;

    Ok((
        HeaderMap::new(),
        Json(MessageResp {
            conversation_id: id,
            turn_index,
            assistant,
            reasoning,
            turn_count,
        }),
    ))
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

    let max_messages = api_max_history();
    let start = record.messages.len().saturating_sub(max_messages);
    let truncated = start > 0;
    let messages: Vec<HistoryMessage> = record
        .messages
        .iter()
        .skip(start)
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
        truncated,
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
        CrewEvent::AgentToolStarted { .. } => "agent_tool_started",
        CrewEvent::AgentToolCompleted { .. } => "agent_tool_completed",
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
    let sse_permit = state.sse_permits.clone().try_acquire_owned().map_err(|_| {
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
            | CrewEvent::AgentToolStarted { .. }
            | CrewEvent::AgentToolCompleted { .. }
    )
}

// ---------------------------------------------------------------------------
// DELETE /flows/{flow}/conversations/{id}
// ---------------------------------------------------------------------------

pub async fn delete_conversation(
    State(state): State<Arc<AppState>>,
    Path((flow, id)): Path<(String, String)>,
    headers: HeaderMap,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    let result: Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> = async {
        let flow_path_resolved =
            resolve_flow_path(&state, &flow).map_err(|e| map_err_to_response(&e))?;
        validate_session_id(&id).map_err(|e| map_err_to_response(&e))?;

        let flow_slug = flow_segment(&flow_path_resolved);
        let key = (flow_slug.clone(), id.clone());
        let lifecycle = lifecycle_lock(&key);
        let _lifecycle_guard = lifecycle.lock().await;

        // Wait for an in-flight turn before deleting its durable record. All
        // same-id operations honor the lifecycle gate, so no cloned stale
        // handle can autosave after this deletion or race a recreated handle.
        let handle = state.active_conversations.read().await.get(&key).cloned();
        let _turn_guard = match handle.as_ref() {
            Some(handle) => Some(handle.turn_lock.lock().await),
            None => None,
        };
        if let Some(handle) = handle.as_ref() {
            let mut map = state.active_conversations.write().await;
            if map
                .get(&key)
                .is_some_and(|current| Arc::ptr_eq(current, handle))
            {
                map.remove(&key);
            }
        }

        // Remove the persisted record, scoped to this flow so a delete
        // can never touch another flow's session with the same id.
        state
            .store
            .delete_conversation(Some(&flow_slug), &id)
            .await
            .map_err(|e| map_err_to_response(&e))?;

        Ok(Json(serde_json::json!({ "deleted": id })))
    }
    .await;

    let (success, status_code) = match &result {
        Ok(_) => (true, 200u16),
        Err((sc, _)) => (false, sc.as_u16()),
    };

    crate::api::audit::record(
        &state.store,
        "conversation.delete",
        Some(&flow),
        Some(&id),
        &headers,
        Some(addr),
        success,
        status_code,
        None,
    )
    .await;

    result
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
        let expired: Vec<(ConversationKey, Arc<ConversationHandle>)> = {
            let map = state.active_conversations.read().await;
            let mut out = Vec::new();
            for (key, handle) in map.iter() {
                let last = *handle.last_touched.read().await;
                if now.saturating_duration_since(last) >= idle_cutoff {
                    out.push((key.clone(), handle.clone()));
                }
            }
            out
        };

        if expired.is_empty() {
            continue;
        }

        // Phase 2 — serialize against same-id start/message/delete, wait for a
        // current turn, and re-check both identity and idle time. This avoids
        // evicting a freshly touched handle or deleting a newly recreated one.
        let mut evicted = 0usize;
        for (key, observed) in expired {
            let lifecycle = lifecycle_lock(&key);
            let _lifecycle_guard = lifecycle.lock().await;
            let _turn_guard = observed.turn_lock.lock().await;

            let is_current = state
                .active_conversations
                .read()
                .await
                .get(&key)
                .is_some_and(|current| Arc::ptr_eq(current, &observed));
            let last = *observed.last_touched.read().await;
            if !is_current || Instant::now().saturating_duration_since(last) < idle_cutoff {
                continue;
            }

            if let Err(error) = observed.conv.persist().await {
                tracing::warn!(
                    conversation_id = %observed.id,
                    %error,
                    "Failed to persist conversation during idle eviction; retaining handle for retry"
                );
                continue;
            }
            let mut map = state.active_conversations.write().await;
            if map
                .get(&key)
                .is_some_and(|current| Arc::ptr_eq(current, &observed))
            {
                map.remove(&key);
                evicted += 1;
            }
        }
        if evicted > 0 {
            tracing::info!(evicted, "Evicted idle chat conversation handles");
        }
    }
}
