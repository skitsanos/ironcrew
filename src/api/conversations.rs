//! Phase-1 Human-in-the-Loop: HTTP conversation endpoints.
//!
//! Wraps `crew:conversation({...})` behind six endpoints under
//! `/flows/{flow}/conversations`. A bounded lifecycle gate serializes local
//! same-id mutations, each live handle serializes turns, and PostgreSQL adds
//! the durable incarnation/revision claim across processes.
//!
//! Session creation is explicit: `POST /start` builds the session and
//! stashes it in `AppState.active_conversations`. `POST /messages` against
//! an unknown id returns 404 (never auto-creates). Overlapping mutations for
//! the same session fail fast instead of retaining an unbounded request queue.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::{
    Json,
    extract::{ConnectInfo, Extension, Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::sse::{Event, Sse},
    response::{IntoResponse, Response},
};
use mlua::AnyUserData;
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, OwnedMutexGuard, OwnedSemaphorePermit, RwLock, broadcast};

use super::admission::QuotaMetric;
use super::auth::Principal;
use super::conversation_lifecycle::{
    ConversationKey, ConversationLifecycleRegistryFull, OwnedConversationLifecycleGuard,
};
use super::{AppState, ErrorResponse, error_response, resolve_flow_path};
use crate::engine::conversation_definition::{FlowSourceSnapshot, capture_flow_source};
use crate::engine::eventbus::{CrewEvent, EventBus};
use crate::engine::idempotency::{
    CONVERSATION_MESSAGE_OPERATION, ConversationIdempotencyCommit, IdempotencyClaim,
    IdempotencyClaimOutcome, IdempotencyCompletion, IdempotencyLookup, IdempotencyQuotaResource,
    IdempotencyQuotaScope, IdempotencyRecord, PrincipalId,
};
use crate::engine::sessions::{ConversationExecution, ConversationRecord, validate_session_id};
use crate::engine::store::ConversationCoordinationScope;
use crate::lua::api::{CHAT_CREW_REGISTRY_KEY, ChatMode, set_ironcrew_mode};
use crate::lua::conversation::{LuaConversation, LuaConversationInner};
use crate::tools::ToolCallContext;
use crate::utils::error::IronCrewError;

mod image_input;
use image_input::load_message_images;

type MessageResult = Result<(HeaderMap, Json<MessageResp>), (StatusCode, Json<ErrorResponse>)>;

#[derive(Clone)]
struct MessageIdempotencyAttempt {
    key_hash: String,
    principal_id: PrincipalId,
    request_fingerprint: String,
    attempt_id: String,
    lease_deadline: tokio::time::Instant,
}

struct ClaimedMessage {
    attempt: MessageIdempotencyAttempt,
    heartbeat: super::idempotency::LeaseHeartbeat,
}

fn replay_message(
    record: &IdempotencyRecord,
    id: &str,
    execution: &ConversationExecution,
) -> MessageResult {
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
    validate_replayed_message(&response, id, execution, record.base_revision)?;
    Ok((super::idempotency::replay_headers(), Json(response)))
}

fn validate_replayed_message(
    response: &MessageResp,
    id: &str,
    execution: &ConversationExecution,
    base_revision: Option<u64>,
) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
    let expected_revision = base_revision.and_then(|revision| revision.checked_add(1));
    if response.conversation_id != id
        || response.incarnation_id != execution.incarnation_id
        || response.definition_fingerprint != execution.definition_fingerprint
        || Some(response.revision) != expected_revision
        || response.turn_count == 0
        || response.turn_index != response.turn_count.saturating_sub(1)
    {
        tracing::error!(
            "Stored conversation idempotency response has invalid identity or revision"
        );
        return Err(error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Stored idempotency response has invalid conversation identity".into(),
        ));
    }
    Ok(())
}

async fn validated_message_replay(
    state: &Arc<AppState>,
    flow: &str,
    id: &str,
    execution: &ConversationExecution,
    record: &IdempotencyRecord,
) -> MessageResult {
    let expected_scope =
        super::idempotency::conversation_scope(flow, id, &execution.incarnation_id);
    if record.operation != CONVERSATION_MESSAGE_OPERATION
        || record.scope != flow
        || record.resource_id != id
        || record.exclusive_scope.as_deref() != Some(expected_scope.as_str())
    {
        tracing::error!("Stored conversation idempotency response has an invalid resource fence");
        return Err(error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Stored idempotency response has an invalid conversation fence".into(),
        ));
    }
    let current = state
        .store
        .get_conversation(Some(flow), id)
        .await
        .map_err(conversation_store_error)?;
    if current
        .as_ref()
        .is_none_or(|record| record.execution != *execution)
    {
        state
            .active_conversations
            .write()
            .await
            .remove(&(flow.to_string(), id.to_string()));
        return Err(error_response(
            StatusCode::CONFLICT,
            "Conversation was deleted or recreated; the prior incarnation cannot be replayed"
                .into(),
        ));
    }
    replay_message(record, id, execution)
}

fn message_idempotency_store_error(error: IronCrewError) -> (StatusCode, Json<ErrorResponse>) {
    tracing::error!(%error, "Conversation idempotency storage operation failed");
    error_response(
        StatusCode::SERVICE_UNAVAILABLE,
        "Idempotency storage is temporarily unavailable".into(),
    )
}

fn observed_message_idempotency_store_error(
    error: IronCrewError,
) -> (StatusCode, Json<ErrorResponse>) {
    crate::metrics::record_store_error(crate::metrics::StoreOperation::Idempotency, &error);
    message_idempotency_store_error(error)
}

fn conversation_store_error(error: IronCrewError) -> (StatusCode, Json<ErrorResponse>) {
    crate::metrics::record_store_error(crate::metrics::StoreOperation::Conversation, &error);
    map_err_to_response(&error)
}

fn message_idempotency_quota_error(
    state: &AppState,
    scope: IdempotencyQuotaScope,
    resource: IdempotencyQuotaResource,
    retry_after_seconds: u64,
) -> (StatusCode, Json<ErrorResponse>) {
    let metric = match (scope, resource) {
        (IdempotencyQuotaScope::Global, IdempotencyQuotaResource::Records) => {
            QuotaMetric::GlobalRecords
        }
        (IdempotencyQuotaScope::Principal, IdempotencyQuotaResource::Records) => {
            QuotaMetric::PrincipalRecords
        }
        (IdempotencyQuotaScope::Principal, IdempotencyQuotaResource::InFlight) => {
            QuotaMetric::PrincipalInFlight
        }
        (IdempotencyQuotaScope::Global, IdempotencyQuotaResource::InFlight) => {
            QuotaMetric::PrincipalInFlight
        }
    };
    state.admission.metrics().record_quota_rejection(metric);
    error_response(
        StatusCode::TOO_MANY_REQUESTS,
        format!(
            "Idempotency capacity is exhausted; retry after at least {} second(s)",
            retry_after_seconds.max(1)
        ),
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

fn lifecycle_capacity_error(
    error: ConversationLifecycleRegistryFull,
) -> (StatusCode, Json<ErrorResponse>) {
    error_response(
        StatusCode::SERVICE_UNAVAILABLE,
        format!(
            "Concurrent conversation lifecycle limit reached ({} distinct conversations); retry shortly or raise IRONCREW_MAX_CONVERSATION_LIFECYCLES",
            error.capacity
        ),
    )
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
    pub revision: u64,
    pub incarnation_id: String,
    pub source_fingerprint: String,
    pub definition_fingerprint: String,
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
    pub revision: u64,
    pub incarnation_id: String,
    pub definition_fingerprint: String,
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
    pub revision: u64,
    pub incarnation_id: String,
    pub source_fingerprint: String,
    pub definition_fingerprint: String,
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
    if let IronCrewError::Lua(error) = e
        && let Some(embedded) = embedded_lua_client_error(error)
    {
        return map_err_to_response(embedded);
    }
    let status = match e {
        IronCrewError::Conflict(_) => StatusCode::CONFLICT,
        IronCrewError::Validation(_) => StatusCode::BAD_REQUEST,
        IronCrewError::Io(_) => StatusCode::INTERNAL_SERVER_ERROR,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    error_response(status, e.to_string())
}

fn embedded_lua_client_error(error: &mlua::Error) -> Option<&IronCrewError> {
    let mut current = error;
    loop {
        if let Some(embedded) = current.downcast_ref::<IronCrewError>()
            && matches!(
                embedded,
                IronCrewError::Validation(_) | IronCrewError::Conflict(_)
            )
        {
            return Some(embedded);
        }
        let parent = current.parent()?;
        current = parent;
    }
}

fn map_lua_err_to_response(error: mlua::Error) -> (StatusCode, Json<ErrorResponse>) {
    map_err_to_response(&IronCrewError::Lua(error))
}

fn flow_segment(path: &std::path::Path) -> String {
    path.file_name()
        .and_then(|s| s.to_str())
        .map(|s| s.to_string())
        .unwrap_or_default()
}

async fn current_flow_source_snapshot(
    flow_path: &std::path::Path,
) -> Result<Arc<FlowSourceSnapshot>, (StatusCode, Json<ErrorResponse>)> {
    let flow_path = flow_path.to_path_buf();
    tokio::task::spawn_blocking(move || capture_flow_source(&flow_path).map(Arc::new))
        .await
        .map_err(|error| {
            tracing::error!(%error, "Conversation source fingerprint worker failed");
            error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to inspect the conversation definition".into(),
            )
        })?
        .map_err(|error| map_err_to_response(&error))
}

fn validate_stored_start(
    record: &ConversationRecord,
    requested_agent: Option<&str>,
    requested_max_history: Option<usize>,
    history_cap: usize,
    source_fingerprint: &str,
) -> Result<(String, usize), (StatusCode, Json<ErrorResponse>)> {
    record
        .execution
        .validate()
        .map_err(|error| map_err_to_response(&error))?;
    if requested_agent.is_some_and(|requested| requested != record.agent_name.as_str()) {
        return Err(error_response(
            StatusCode::CONFLICT,
            "start: requested `agent` does not match the stored conversation agent".into(),
        ));
    }
    if requested_max_history.is_some_and(|requested| requested != record.execution.max_history) {
        return Err(error_response(
            StatusCode::CONFLICT,
            "start: requested `max_history` does not match the stored conversation limit".into(),
        ));
    }
    if record.execution.max_history > history_cap {
        return Err(error_response(
            StatusCode::CONFLICT,
            "The stored conversation history limit exceeds the current HTTP policy".into(),
        ));
    }
    if record.execution.source_fingerprint != source_fingerprint {
        return Err(error_response(
            StatusCode::CONFLICT,
            "Conversation flow source changed; restore the original definition or start a new conversation"
                .into(),
        ));
    }
    Ok((record.agent_name.clone(), record.execution.max_history))
}

async fn start_response(
    handle: &ConversationHandle,
    flow: String,
    events_url: String,
) -> Json<StartResp> {
    Json(StartResp {
        conversation_id: handle.id.clone(),
        flow,
        agent: handle.agent.clone(),
        created_at: handle.created_at.clone(),
        turn_count: handle.conv.turn_count().await,
        revision: handle.conv.revision().await,
        incarnation_id: handle.conv.execution.incarnation_id.clone(),
        source_fingerprint: handle.conv.execution.source_fingerprint.clone(),
        definition_fingerprint: handle.conv.execution.definition_fingerprint.clone(),
        events_url,
    })
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

    if !state.lifecycle.is_accepting_mutations() {
        return Err(error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "Server is shutting down".into(),
        ));
    }

    let flow_slug = flow_segment(&flow_path_resolved);
    let key = (flow_slug.clone(), id.clone());
    let lifecycle = state
        .conversation_lifecycles
        .acquire(&key)
        .map_err(lifecycle_capacity_error)?;
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

    let requested_agent = req
        .agent
        .as_deref()
        .map(str::trim)
        .filter(|agent| !agent.is_empty())
        .map(str::to_owned);
    let mut stored = state
        .store
        .get_conversation(Some(&flow_slug), &id)
        .await
        .map_err(conversation_store_error)?;

    // Agent and limit conflicts are checked before reading or executing the
    // current flow, so a mismatched resume cannot trigger Lua side effects.
    if let Some(record) = stored.as_ref() {
        record
            .execution
            .validate()
            .map_err(|error| map_err_to_response(&error))?;
        if requested_agent
            .as_deref()
            .is_some_and(|requested| requested != record.agent_name.as_str())
        {
            return Err(error_response(
                StatusCode::CONFLICT,
                "start: requested `agent` does not match the stored conversation agent".into(),
            ));
        }
        if req
            .max_history
            .is_some_and(|requested| requested != record.execution.max_history)
        {
            return Err(error_response(
                StatusCode::CONFLICT,
                "start: requested `max_history` does not match the stored conversation limit"
                    .into(),
            ));
        }
    }

    let source_snapshot = current_flow_source_snapshot(&flow_path_resolved).await?;
    let source_fingerprint = source_snapshot.fingerprint();
    if stored.is_none() {
        // A peer may have deleted the durable incarnation while this process
        // still held a cache entry. Never let that stale handle authorize a
        // restart or retain an admission permit.
        state.active_conversations.write().await.remove(&key);
    }
    let (agent_name, max_history) = match stored.as_ref() {
        Some(record) => validate_stored_start(
            record,
            requested_agent.as_deref(),
            req.max_history,
            history_cap,
            source_fingerprint,
        )?,
        None => (
            requested_agent.clone().ok_or_else(|| {
                error_response(
                    StatusCode::BAD_REQUEST,
                    "start: `agent` is required for a new conversation".into(),
                )
            })?,
            req.max_history.unwrap_or(history_cap),
        ),
    };
    let build_req = StartReq {
        agent: Some(agent_name),
        max_history: Some(max_history),
    };
    let events_url = format!("/flows/{}/conversations/{}/events", flow, id);

    // A live handle is only a cache. Verify it still represents the exact
    // durable incarnation, definition, and revision before returning it.
    let existing = state.active_conversations.read().await.get(&key).cloned();
    if let Some(existing) = existing {
        let existing_revision = existing.conv.revision().await;
        let current = stored.as_ref().is_some_and(|record| {
            existing.conv.execution == record.execution && existing_revision == record.revision
        });
        if current {
            return Ok(start_response(&existing, flow, events_url).await);
        }
        let mut map = state.active_conversations.write().await;
        if map
            .get(&key)
            .is_some_and(|current| Arc::ptr_eq(current, &existing))
        {
            map.remove(&key);
        }
        drop(map);
        drop(existing);
    }

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
    let (mut handle, _, _) = build_session(
        &state,
        &flow_path_resolved,
        &flow_slug,
        &id,
        &build_req,
        source_snapshot.clone(),
        admission_permit,
    )
    .await?;

    let built_revision = handle.conv.revision().await;
    if let Some(expected) = stored.as_ref() {
        if handle.conv.execution != expected.execution || built_revision != expected.revision {
            return Err(error_response(
                StatusCode::CONFLICT,
                "Conversation changed while its live handle was being rebuilt; retry /start".into(),
            ));
        }
    } else if built_revision != 0 {
        // `build_session` performs its own durable lookup. If another process
        // created this id after our first read, adopt that exact winner as a
        // read-only resume instead of incrementing its revision as though it
        // were our fresh bootstrap.
        let winner = state
            .store
            .get_conversation(Some(&flow_slug), &id)
            .await
            .map_err(conversation_store_error)?
            .ok_or_else(|| {
                error_response(
                    StatusCode::CONFLICT,
                    "Conversation changed while its live handle was being rebuilt; retry /start"
                        .into(),
                )
            })?;
        if handle.conv.execution != winner.execution || built_revision != winner.revision {
            return Err(error_response(
                StatusCode::CONFLICT,
                "Conversation changed while its live handle was being rebuilt; retry /start".into(),
            ));
        }
        stored = Some(winner);
    }

    // A resume is read-only. For a fresh start, establish durability before
    // publishing. If another process created the same id first, discard this
    // candidate and rebuild once from the winning durable incarnation.
    if stored.is_none()
        && let Err(error) = handle.conv.persist().await
    {
        if !matches!(error, IronCrewError::Conflict(_)) {
            crate::metrics::record_store_error(
                crate::metrics::StoreOperation::Conversation,
                &error,
            );
            return Err(error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to persist conversation bootstrap: {error}"),
            ));
        }
        drop(handle);
        stored = state
            .store
            .get_conversation(Some(&flow_slug), &id)
            .await
            .map_err(conversation_store_error)?;
        let winner = stored.as_ref().ok_or_else(|| {
            error_response(
                StatusCode::CONFLICT,
                "Conversation creation raced another request; retry /start".into(),
            )
        })?;
        let (winner_agent, winner_max_history) = validate_stored_start(
            winner,
            requested_agent.as_deref(),
            req.max_history,
            history_cap,
            source_fingerprint,
        )?;
        let permit = state
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
        handle = build_session(
            &state,
            &flow_path_resolved,
            &flow_slug,
            &id,
            &StartReq {
                agent: Some(winner_agent),
                max_history: Some(winner_max_history),
            },
            source_snapshot.clone(),
            permit,
        )
        .await?
        .0;
    }

    // Resolve a same-id creation race under the map lock. Only the winning
    // handle is published and returned; the losing candidate is dropped,
    // which also returns its unused admission permit. The lifecycle lock
    // normally makes the occupied branch unreachable, but the map check keeps
    // this invariant robust to future callers.
    let selected = {
        use std::collections::hash_map::Entry;
        let mut map = state.active_conversations.write().await;
        if !state.lifecycle.is_accepting_mutations() {
            return Err(error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                "Server is shutting down".into(),
            ));
        }
        match map.entry(key) {
            Entry::Vacant(entry) => {
                entry.insert(handle.clone());
                handle.clone()
            }
            Entry::Occupied(entry) => entry.get().clone(),
        }
    };
    Ok(start_response(&selected, flow, events_url).await)
}

/// Helper — build the Lua VM + conversation inner, wrap in a
/// `ConversationHandle`, and return.
async fn build_session(
    state: &Arc<AppState>,
    flow_path: &std::path::Path,
    flow_slug: &str,
    id: &str,
    req: &StartReq,
    source_snapshot: Arc<FlowSourceSnapshot>,
    admission_permit: OwnedSemaphorePermit,
) -> Result<(Arc<ConversationHandle>, String, usize), (StatusCode, Json<ErrorResponse>)> {
    use crate::cli::project::setup_http_conversation_runtime;

    let loader = crate::lua::loader::ProjectLoader::from_conversation_snapshot(&source_snapshot)
        .map_err(|e| map_err_to_response(&e))?;
    let (lua, _runtime, entrypoint) =
        setup_http_conversation_runtime(&loader, source_snapshot.clone())
            .map_err(|e| map_err_to_response(&e))?;
    let source_fingerprint = source_snapshot.fingerprint();

    // Mark chat mode so the Crew constructor parks its userdata in the
    // registry AND so user code can guard `crew:run()` appropriately.
    lua.set_app_data(ChatMode);
    // Share the server-wide store with the Lua VM so the LuaCrew
    // constructor reuses it instead of calling `create_store()` again
    // (which would re-run Postgres bootstrap on every session start).
    lua.set_app_data(state.store.clone());
    lua.set_app_data(crate::lua::conversation::ConversationSourceFingerprint(
        source_fingerprint.to_string(),
    ));
    set_ironcrew_mode(&lua, "chat").map_err(|e| map_err_to_response(&IronCrewError::Lua(e)))?;

    // Per-session event bus. Conversation turn events flow through this bus.
    let eventbus = EventBus::new(256);
    lua.set_app_data(eventbus.clone());

    // Execute the exact entrypoint bytes that were fingerprinted.
    let entrypoint_result = {
        let _execution = crate::lua::limits::LuaExecutionGuard::begin(&lua)
            .map_err(|e| map_err_to_response(&IronCrewError::Lua(e)))?;
        lua.load(entrypoint.source())
            .set_name(format!(
                "@snapshot/{}",
                entrypoint.relative_path().display()
            ))
            .exec_async()
            .await
    };
    lua.remove_app_data::<crate::lua::bootstrap::HttpConversationBootstrap>();
    entrypoint_result.map_err(map_lua_err_to_response)?;

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
            .map_err(map_lua_err_to_response)?
    };

    let conv: Arc<LuaConversationInner> = {
        let wrapper = conv_ud
            .borrow::<LuaConversation>()
            .map_err(map_lua_err_to_response)?;
        wrapper.inner()
    };

    // Bind the durable definition to one stable source-tree observation.
    // The first fingerprint is threaded into Lua's definition fingerprint;
    // this second bounded walk detects a rollout or local edit that crossed
    // session construction instead of publishing a mixed-definition handle.
    let verified_source = current_flow_source_snapshot(flow_path).await?;
    let verified_source_fingerprint = verified_source.fingerprint();
    if verified_source_fingerprint != source_fingerprint {
        return Err(error_response(
            StatusCode::CONFLICT,
            "Conversation flow source changed while the session was being built; retry after the deployment stabilizes"
                .into(),
        ));
    }

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

async fn message_handle(
    state: &Arc<AppState>,
    flow_path: &std::path::Path,
    flow_slug: &str,
    id: &str,
    record: &ConversationRecord,
    source_snapshot: Arc<FlowSourceSnapshot>,
) -> Result<Arc<ConversationHandle>, (StatusCode, Json<ErrorResponse>)> {
    let key = (flow_slug.to_string(), id.to_string());
    let existing = state.active_conversations.read().await.get(&key).cloned();
    let mut invalidated_stale_handle = false;
    if let Some(existing) = existing {
        let revision = existing.conv.revision().await;
        if existing.conv.execution == record.execution && revision == record.revision {
            return Ok(existing);
        }
        let mut map = state.active_conversations.write().await;
        if map
            .get(&key)
            .is_some_and(|current| Arc::ptr_eq(current, &existing))
        {
            map.remove(&key);
            invalidated_stale_handle = true;
        }
        drop(map);
        drop(existing);
    }

    if state.store.conversation_coordination_scope() != ConversationCoordinationScope::SharedStore {
        if invalidated_stale_handle {
            return Err(error_response(
                StatusCode::CONFLICT,
                "Conversation changed in durable storage; call /start to reload it before retrying"
                    .into(),
            ));
        }
        return Err(error_response(
            StatusCode::NOT_FOUND,
            format!("Conversation '{id}' is not active — call /start first"),
        ));
    }

    let permit = state
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
    let handle = build_session(
        state,
        flow_path,
        flow_slug,
        id,
        &StartReq {
            agent: Some(record.agent_name.clone()),
            max_history: Some(record.execution.max_history),
        },
        source_snapshot,
        permit,
    )
    .await?
    .0;
    let revision = handle.conv.revision().await;
    if handle.conv.execution != record.execution || revision != record.revision {
        return Err(error_response(
            StatusCode::CONFLICT,
            "Conversation changed while its live handle was being rebuilt; retry the message"
                .into(),
        ));
    }
    state
        .active_conversations
        .write()
        .await
        .insert(key, handle.clone());
    Ok(handle)
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
                crate::metrics::record_terminal_persistence(
                    crate::metrics::TerminalScope::ConversationIndeterminate,
                    crate::metrics::TerminalOutcome::from_applied(updated),
                );
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
                crate::metrics::record_terminal_persistence(
                    crate::metrics::TerminalScope::ConversationIndeterminate,
                    crate::metrics::TerminalOutcome::Fenced,
                );
                tracing::warn!(%error, "Indeterminate conversation finalization was fenced");
                if persistence_degraded {
                    state
                        .terminal_persistence_failures
                        .fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
                }
                return;
            }
            Err(error) => {
                crate::metrics::record_terminal_persistence(
                    crate::metrics::TerminalScope::ConversationIndeterminate,
                    crate::metrics::TerminalOutcome::Error,
                );
                crate::metrics::record_store_failure(
                    crate::metrics::StoreOperation::TerminalPersistence,
                );
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

async fn release_message_claim(state: &Arc<AppState>, attempt: &MessageIdempotencyAttempt) {
    if let Err(error) = state
        .store
        .release_idempotency(&attempt.key_hash, &attempt.attempt_id)
        .await
    {
        crate::metrics::record_store_error(crate::metrics::StoreOperation::Idempotency, &error);
        tracing::error!(%error, "Failed to release a conversation idempotency claim before provider execution");
    }
}

async fn finish_claim_after_preparation_failure(
    state: &Arc<AppState>,
    key: &ConversationKey,
    claimed: ClaimedMessage,
    may_have_executed_flow_code: bool,
) {
    drop(claimed.heartbeat);
    if may_have_executed_flow_code {
        state.active_conversations.write().await.remove(key);
        mark_message_indeterminate(state, &claimed.attempt).await;
    } else {
        release_message_claim(state, &claimed.attempt).await;
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
            .commit_conversation_idempotency_with_limits(
                completion.clone(),
                conversation,
                state.idempotency.limits(),
            )
            .await
        {
            Ok(committed) => {
                crate::metrics::record_terminal_persistence(
                    crate::metrics::TerminalScope::ConversationCommit,
                    crate::metrics::TerminalOutcome::from_applied(!committed.already_completed),
                );
                if persistence_degraded {
                    state
                        .terminal_persistence_failures
                        .fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
                }
                return Ok(committed);
            }
            Err(error @ IronCrewError::Conflict(_)) => {
                crate::metrics::record_terminal_persistence(
                    crate::metrics::TerminalScope::ConversationCommit,
                    crate::metrics::TerminalOutcome::Fenced,
                );
                if persistence_degraded {
                    state
                        .terminal_persistence_failures
                        .fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
                }
                return Err(error);
            }
            Err(error) => {
                crate::metrics::record_terminal_persistence(
                    crate::metrics::TerminalScope::ConversationCommit,
                    crate::metrics::TerminalOutcome::Error,
                );
                crate::metrics::record_store_failure(
                    crate::metrics::StoreOperation::TerminalPersistence,
                );
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
    image_paths: Option<Vec<String>>,
    flow_path: std::path::PathBuf,
    attempt: MessageIdempotencyAttempt,
    heartbeat: super::idempotency::LeaseHeartbeat,
    _lifecycle_guard: OwnedConversationLifecycleGuard,
    _turn_guard: OwnedMutexGuard<()>,
) -> Result<MessageResp, (StatusCode, Json<ErrorResponse>)> {
    let mut claim_loss = heartbeat.loss_receiver();
    let mut shutdown = handle.shutdown.subscribe();
    let shared_store =
        state.store.conversation_coordination_scope() == ConversationCoordinationScope::SharedStore;
    let image_load = load_message_images(&handle, &flow_path, image_paths, shared_store);
    tokio::pin!(image_load);
    let images = tokio::select! {
        biased;
        _ = shutdown.changed() => {
            drop(heartbeat);
            release_message_claim(&state, &attempt).await;
            return Err(error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                "Conversation turn cancelled during server shutdown".into(),
            ));
        }
        _ = super::idempotency::wait_for_lease_loss(&mut claim_loss) => {
            drop(heartbeat);
            state.active_conversations.write().await.remove(&key);
            mark_message_indeterminate(&state, &attempt).await;
            return Err(error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                "Conversation image preparation stopped after its idempotency claim was lost; inspect history before retrying"
                    .into(),
            ));
        }
        result = &mut image_load => match result {
            Ok(images) => images,
            Err(error) => {
                drop(heartbeat);
                release_message_claim(&state, &attempt).await;
                return Err(error);
            }
        },
    };
    let turn_timeout = handle.conv.tool_registry.conversation_turn_timeout();
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

    let response_revision = prepared.record.revision.checked_add(1).ok_or_else(|| {
        error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Conversation revision overflow".into(),
        )
    })?;
    let response = MessageResp {
        conversation_id: id,
        turn_index: prepared.turn_index,
        assistant: prepared.assistant.clone(),
        reasoning: prepared.reasoning.clone(),
        turn_count: prepared.turn_count,
        revision: response_revision,
        incarnation_id: prepared.record.execution.incarnation_id.clone(),
        definition_fingerprint: prepared.record.execution.definition_fingerprint.clone(),
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
        principal_id: attempt.principal_id.clone(),
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
    Extension(principal): Extension<Principal>,
    Path((flow, id)): Path<(String, String)>,
    headers: HeaderMap,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Json(req): Json<MessageReq>,
) -> MessageResult {
    if !state.lifecycle.is_accepting_mutations() {
        return Err(error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "Server is shutting down".into(),
        ));
    }
    let flow_path_resolved =
        resolve_flow_path(&state, &flow).map_err(|e| map_err_to_response(&e))?;
    validate_session_id(&id).map_err(|e| map_err_to_response(&e))?;

    let shared =
        state.store.conversation_coordination_scope() == ConversationCoordinationScope::SharedStore;
    let request_key = super::idempotency::request_key(
        &headers,
        state.idempotency.require_key || shared,
        principal.id(),
    )
    .map_err(|error| error_response(StatusCode::BAD_REQUEST, error.to_string()))?;
    let recovery_key = super::idempotency::recovery_key(&headers, principal.id())
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
    let durable = state
        .store
        .get_conversation(Some(&flow_slug), &id)
        .await
        .map_err(conversation_store_error)?;
    let Some(durable) = durable else {
        state
            .active_conversations
            .write()
            .await
            .remove(&(flow_slug.clone(), id.clone()));
        return Err(error_response(
            StatusCode::NOT_FOUND,
            format!("Conversation '{id}' not found"),
        ));
    };
    durable
        .execution
        .validate()
        .map_err(|error| map_err_to_response(&error))?;
    let request_fingerprint = super::idempotency::conversation_message_fingerprint(
        &flow_slug,
        &id,
        &durable.execution.incarnation_id,
        &req.content,
        req.images.as_deref(),
    );

    if let Some(request_key) = request_key.as_ref() {
        let now = chrono::Utc::now().to_rfc3339();
        match state
            .store
            .lookup_idempotency_for_principal(
                principal.id(),
                &request_key.key_hash,
                &request_fingerprint,
                &now,
            )
            .await
            .map_err(observed_message_idempotency_store_error)?
        {
            IdempotencyLookup::Miss => {}
            IdempotencyLookup::Replay(record) => {
                return validated_message_replay(
                    &state,
                    &flow_slug,
                    &id,
                    &durable.execution,
                    &record,
                )
                .await;
            }
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

    let source_snapshot = current_flow_source_snapshot(&flow_path_resolved).await?;
    if durable.execution.source_fingerprint != source_snapshot.fingerprint() {
        state
            .active_conversations
            .write()
            .await
            .remove(&(flow_slug.clone(), id.clone()));
        return Err(error_response(
            StatusCode::CONFLICT,
            "Conversation flow source changed; restore the original definition or start a new conversation"
                .into(),
        ));
    }

    let key = (flow_slug, id.clone());
    let lifecycle = state
        .conversation_lifecycles
        .acquire(&key)
        .map_err(lifecycle_capacity_error)?;
    let lifecycle_guard = lifecycle.try_lock_owned().map_err(|_| {
        error_response(
            StatusCode::CONFLICT,
            "Conversation is busy; retry after the active operation completes".into(),
        )
    })?;
    if !state.lifecycle.is_accepting_mutations() {
        return Err(error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "Server is shutting down".into(),
        ));
    }
    let mut claimed = if let Some(request_key) = request_key {
        let scope =
            super::idempotency::conversation_scope(&key.0, &id, &durable.execution.incarnation_id);
        let lease_started = tokio::time::Instant::now();
        let now = chrono::Utc::now();
        let lease_ttl = state.store.run_lease_ttl();
        let attempt_id = uuid::Uuid::new_v4().to_string();
        let lease_expires_at = now
            .checked_add_signed(
                chrono::Duration::from_std(lease_ttl)
                    .unwrap_or_else(|_| chrono::Duration::seconds(60)),
            )
            .unwrap_or(chrono::DateTime::<chrono::Utc>::MAX_UTC)
            .to_rfc3339();
        let claim = IdempotencyClaim {
            key_hash: request_key.key_hash.clone(),
            principal_id: principal.id().clone(),
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
            base_revision: Some(durable.revision),
            response_status: None,
            response_body: None,
            max_total_response_bytes: state.idempotency.max_total_response_bytes,
            lease_expires_at,
            created_at: now.to_rfc3339(),
            ttl_seconds: state.idempotency.ttl_seconds,
        };
        let attempt = MessageIdempotencyAttempt {
            key_hash: request_key.key_hash,
            principal_id: principal.id().clone(),
            request_fingerprint,
            attempt_id,
            lease_deadline: super::idempotency::conservative_lease_deadline(
                lease_started,
                lease_ttl,
            ),
        };
        match state
            .store
            .claim_idempotency_with_limits(claim, state.idempotency.limits())
            .await
            .map_err(observed_message_idempotency_store_error)?
        {
            IdempotencyClaimOutcome::Claimed(_) => {}
            IdempotencyClaimOutcome::Replay(record) => {
                return validated_message_replay(&state, &key.0, &id, &durable.execution, &record)
                    .await;
            }
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
            IdempotencyClaimOutcome::QuotaExceeded {
                scope,
                resource,
                retry_after_seconds,
            } => {
                return Err(message_idempotency_quota_error(
                    &state,
                    scope,
                    resource,
                    retry_after_seconds,
                ));
            }
        }

        if tokio::time::Instant::now() >= attempt.lease_deadline {
            if let Err(error) = state
                .store
                .release_idempotency(&attempt.key_hash, &attempt.attempt_id)
                .await
            {
                crate::metrics::record_store_error(
                    crate::metrics::StoreOperation::Idempotency,
                    &error,
                );
                tracing::error!(%error, "Failed to release an expired conversation idempotency claim");
            }
            return Err(error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                "The durable message claim consumed its lease window before execution could start; retry with the same Idempotency-Key"
                    .into(),
            ));
        }

        let heartbeat = super::idempotency::LeaseHeartbeat::spawn(
            state.store.clone(),
            attempt.key_hash.clone(),
            attempt.attempt_id.clone(),
            CONVERSATION_MESSAGE_OPERATION,
            attempt.lease_deadline,
        );
        Some(ClaimedMessage { attempt, heartbeat })
    } else {
        None
    };

    // In shared-store mode, construction of a cold Lua handle runs only
    // after the durable mutation claim is active. This keeps top-level chat
    // setup, provider selection, and tool finalization behind the same fence
    // as the turn itself instead of allowing two replicas to build first and
    // race at the later transcript commit.
    enum HandleBuildOutcome {
        Finished(Result<Arc<ConversationHandle>, (StatusCode, Json<ErrorResponse>)>),
        ClaimLost,
    }
    let handle_outcome = {
        let handle_build = message_handle(
            &state,
            &flow_path_resolved,
            &key.0,
            &id,
            &durable,
            source_snapshot,
        );
        tokio::pin!(handle_build);
        if let Some(active_claim) = claimed.as_ref() {
            let mut claim_loss = active_claim.heartbeat.loss_receiver();
            tokio::select! {
                biased;
                _ = super::idempotency::wait_for_lease_loss(&mut claim_loss) => {
                    HandleBuildOutcome::ClaimLost
                }
                result = &mut handle_build => HandleBuildOutcome::Finished(result),
            }
        } else {
            HandleBuildOutcome::Finished(handle_build.await)
        }
    };
    let handle = match handle_outcome {
        HandleBuildOutcome::Finished(Ok(handle)) => handle,
        HandleBuildOutcome::Finished(Err(error)) => {
            if let Some(active_claim) = claimed.take() {
                finish_claim_after_preparation_failure(&state, &key, active_claim, shared).await;
            }
            return Err(error);
        }
        HandleBuildOutcome::ClaimLost => {
            let active_claim = claimed
                .take()
                .expect("claim-loss branch requires an active message claim");
            finish_claim_after_preparation_failure(&state, &key, active_claim, true).await;
            return Err(error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                "Conversation rehydration stopped after its idempotency claim was lost; inspect history before retrying"
                    .into(),
            ));
        }
    };

    let message_policy = handle.conv.tool_registry.conversation_policy();
    let message_error = if req.content.trim().is_empty() {
        Some(error_response(
            StatusCode::BAD_REQUEST,
            "`content` is required".into(),
        ))
    } else if req.content.len() > message_policy.message_bytes() {
        Some(error_response(
            StatusCode::PAYLOAD_TOO_LARGE,
            format!(
                "`content` is {} bytes, exceeds IRONCREW_API_MESSAGE_MAX_BYTES ({})",
                req.content.len(),
                message_policy.message_bytes()
            ),
        ))
    } else {
        None
    };
    if let Some(error) = message_error {
        if let Some(active_claim) = claimed.take() {
            finish_claim_after_preparation_failure(&state, &key, active_claim, shared).await;
        }
        return Err(error);
    }

    // The lifecycle gate prevents delete/eviction/recreation from crossing
    // this turn. Fail fast if another path already owns the handle instead of
    // retaining parsed requests in an unbounded same-session queue.
    let turn_guard = match handle.turn_lock.clone().try_lock_owned() {
        Ok(guard) => guard,
        Err(_) => {
            if let Some(active_claim) = claimed.take() {
                finish_claim_after_preparation_failure(&state, &key, active_claim, shared).await;
            }
            return Err(error_response(
                StatusCode::CONFLICT,
                "Conversation is busy; retry after the active operation completes".into(),
            ));
        }
    };
    if !state.lifecycle.is_accepting_mutations() {
        if let Some(active_claim) = claimed.take() {
            finish_claim_after_preparation_failure(&state, &key, active_claim, shared).await;
        }
        return Err(error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "Server is shutting down".into(),
        ));
    }
    if let Some(ClaimedMessage { attempt, heartbeat }) = claimed {
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
                req.images,
                flow_path_resolved,
                attempt,
                heartbeat,
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

    let images = load_message_images(&handle, &flow_path_resolved, req.images, shared).await?;
    *handle.last_touched.write().await = Instant::now();

    let turn_timeout = handle.conv.tool_registry.conversation_turn_timeout();
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
    let revision = handle.conv.revision().await;

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
            revision,
            incarnation_id: handle.conv.execution.incarnation_id.clone(),
            definition_fingerprint: handle.conv.execution.definition_fingerprint.clone(),
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
        .map_err(conversation_store_error)?
        .ok_or_else(|| {
            error_response(
                StatusCode::NOT_FOUND,
                format!("Conversation '{}' not found", id),
            )
        })?;

    let turn_count = record.messages.iter().filter(|m| m.role == "user").count();

    let max_messages = record.execution.max_history;
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
        revision: record.revision,
        incarnation_id: record.execution.incarnation_id,
        source_fingerprint: record.execution.source_fingerprint,
        definition_fingerprint: record.execution.definition_fingerprint,
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
    Ok(super::sse::hardened_response(response))
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
        let lifecycle = state
            .conversation_lifecycles
            .acquire(&key)
            .map_err(lifecycle_capacity_error)?;
        let _lifecycle_guard = lifecycle.try_lock().map_err(|_| {
            error_response(
                StatusCode::CONFLICT,
                "Conversation is busy; retry after the active operation completes".into(),
            )
        })?;

        // The lifecycle try-lock above has already rejected an active same-id
        // operation. Taking the turn lock closes the cache lookup/removal race
        // so no cloned stale handle can autosave after deletion or recreation.
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
            .map_err(conversation_store_error)?;

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
        .map_err(conversation_store_error)?;
    let total = state
        .store
        .count_conversations(Some(&flow_slug))
        .await
        .map_err(conversation_store_error)?;

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
            let lifecycle = match state.conversation_lifecycles.acquire(&key) {
                Ok(lifecycle) => lifecycle,
                Err(error) => {
                    tracing::warn!(
                        capacity = error.capacity,
                        conversation_id = %observed.id,
                        "Skipping idle eviction while the conversation lifecycle registry is full"
                    );
                    continue;
                }
            };
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

            // HTTP start/message completion already persists the exact
            // conversation revision before exposing success. Saving again
            // here would manufacture a revision during cache eviction and a
            // stale replica could then conflict forever after a peer turn.
            // This cache is therefore clean-by-contract: eviction only drops
            // the process-local handle and its admission permit.
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

#[cfg(test)]
mod lua_error_mapping_tests {
    use std::sync::Arc;

    use super::*;

    fn callback_error(error: IronCrewError) -> mlua::Error {
        mlua::Error::CallbackError {
            traceback: "test traceback".into(),
            cause: Arc::new(mlua::Error::external(error)),
        }
    }

    #[test]
    fn embedded_validation_and_conflict_keep_client_statuses() {
        let (status, Json(body)) = map_lua_err_to_response(callback_error(
            IronCrewError::Validation("invalid bootstrap".into()),
        ));
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body.error, "Validation error: invalid bootstrap");

        let (status, Json(body)) = map_lua_err_to_response(callback_error(
            IronCrewError::Conflict("definition drift".into()),
        ));
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body.error, "Conflict: definition drift");
    }

    #[test]
    fn replay_response_must_match_conversation_identity_and_revision() {
        let execution = ConversationExecution::new(
            format!("sha256:{}", "a".repeat(64)),
            format!("sha256:{}", "b".repeat(64)),
            20,
            1024,
        )
        .unwrap();
        let valid = MessageResp {
            conversation_id: "chat".into(),
            turn_index: 0,
            assistant: "answer".into(),
            reasoning: None,
            turn_count: 1,
            revision: 8,
            incarnation_id: execution.incarnation_id.clone(),
            definition_fingerprint: execution.definition_fingerprint.clone(),
        };
        assert!(validate_replayed_message(&valid, "chat", &execution, Some(7)).is_ok());

        let mut invalid = valid.clone();
        invalid.conversation_id = "other".into();
        assert!(validate_replayed_message(&invalid, "chat", &execution, Some(7)).is_err());
        invalid = valid.clone();
        invalid.incarnation_id = uuid::Uuid::new_v4().to_string();
        assert!(validate_replayed_message(&invalid, "chat", &execution, Some(7)).is_err());
        invalid = valid.clone();
        invalid.definition_fingerprint = format!("sha256:{}", "c".repeat(64));
        assert!(validate_replayed_message(&invalid, "chat", &execution, Some(7)).is_err());
        invalid = valid;
        invalid.revision = 9;
        assert!(validate_replayed_message(&invalid, "chat", &execution, Some(7)).is_err());
    }
}
