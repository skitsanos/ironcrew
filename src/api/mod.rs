pub mod auth;
pub mod conversations;
pub mod handlers;

use axum::{
    Router,
    http::StatusCode,
    response::Json,
    routing::{delete, get, post},
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::engine::eventbus::EventBus;
use crate::engine::store::StateStore;

/// A running crew: its event bus and an abort handle to cancel it.
pub struct ActiveRun {
    pub eventbus: EventBus,
    pub abort_handle: tokio::task::AbortHandle,
}

/// Map of live chat sessions keyed by `(flow_slug, conversation_id)`.
/// Flow slug is the last path segment of the resolved flow dir — the same
/// value stored in `ConversationRecord.flow_path`, so the map is implicitly
/// namespaced by flow.
pub type ActiveConversationsMap =
    Arc<RwLock<HashMap<(String, String), Arc<conversations::ConversationHandle>>>>;

/// Shared application state
pub struct AppState {
    pub flows_dir: PathBuf,
    pub active_runs: Arc<RwLock<HashMap<String, ActiveRun>>>,
    pub active_conversations: ActiveConversationsMap,
    /// Hard cap on `active_conversations.len()` — reads
    /// `IRONCREW_MAX_ACTIVE_CONVERSATIONS` once at boot.
    pub max_active_conversations: usize,
    /// Server-wide persistence singleton. Bootstrapped once at
    /// `cmd_serve` startup and reused across every handler so Postgres
    /// migrations / table checks don't re-run per request, and so every
    /// caller shares the same connection pool instead of spinning a new
    /// one per conversation start.
    pub store: Arc<dyn StateStore>,
}

/// Response from running a crew
#[derive(Serialize)]
pub struct RunCrewResponse {
    pub run_id: String,
    pub flow_name: String,
    pub status: String,
    pub duration_ms: u64,
    /// Aggregate token usage across all tasks in this run.
    pub total_tokens: u32,
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

/// Query params for listing runs.
///
/// Pagination defaults come from `IRONCREW_RUNS_DEFAULT_LIMIT` (default 20);
/// `limit` is hard-capped at `IRONCREW_RUNS_MAX_LIMIT` (default 100) so a
/// single client can't request an unbounded page.
#[derive(Deserialize)]
pub struct ListRunsQuery {
    pub status: Option<String>,
    pub tag: Option<String>,
    pub since: Option<String>,
    pub limit: Option<usize>,
    pub offset: Option<usize>,
}

/// Paginated response for `GET /flows/{flow}/runs`.
#[derive(Serialize)]
pub struct ListRunsResponse {
    pub runs: Vec<crate::engine::run_history::RunSummary>,
    pub total: u64,
    pub limit: usize,
    pub offset: usize,
}

/// Error response
#[derive(Serialize)]
pub struct ErrorResponse {
    pub error: String,
}

pub fn error_response(status: StatusCode, message: String) -> (StatusCode, Json<ErrorResponse>) {
    (status, Json(ErrorResponse { error: message }))
}

/// Resolve a flow path with traversal prevention.
pub fn resolve_flow_path(state: &AppState, flow: &str) -> crate::utils::error::Result<PathBuf> {
    use crate::utils::error::IronCrewError;

    let flow_path = Path::new(flow);
    if flow_path.as_os_str().is_empty()
        || flow_path.is_absolute()
        || flow_path.components().count() != 1
        || flow_path.components().any(|c| {
            matches!(
                c,
                std::path::Component::ParentDir
                    | std::path::Component::RootDir
                    | std::path::Component::Prefix(_)
                    | std::path::Component::CurDir
            )
        })
    {
        return Err(IronCrewError::Validation("Invalid flow identifier".into()));
    }

    let candidate = state.flows_dir.join(flow);
    if !candidate.exists() {
        return Err(IronCrewError::Validation(format!(
            "Flow not found: {}",
            flow
        )));
    }

    let base = state
        .flows_dir
        .canonicalize()
        .unwrap_or_else(|_| state.flows_dir.clone());
    let canonical = candidate.canonicalize().map_err(|e| {
        IronCrewError::Validation(format!("Failed to resolve flow '{}': {}", flow, e))
    })?;

    if !canonical.starts_with(&base) {
        return Err(IronCrewError::Validation(format!(
            "Invalid flow identifier: {}",
            flow
        )));
    }

    Ok(canonical)
}

/// Resolve the `.ironcrew` directory for a given flow (used by `create_store`).
pub fn resolve_ironcrew_dir(state: &AppState, flow: &str) -> crate::utils::error::Result<PathBuf> {
    Ok(resolve_flow_path(state, flow)?.join(".ironcrew"))
}

/// Build the router
pub fn create_router(state: Arc<AppState>) -> Router {
    use handlers::*;

    // Public routes (no auth required)
    let public = Router::new().route("/health", get(health));

    // Protected routes (auth required when IRONCREW_API_TOKEN is set)
    let protected = Router::new()
        .route("/flows/{flow}/run", post(run_flow))
        .route("/flows/{flow}/abort/{run_id}", post(abort_run))
        .route("/flows/{flow}/runs", get(list_runs))
        .route("/flows/{flow}/runs/{id}", get(get_run))
        .route("/flows/{flow}/runs/{id}", delete(delete_run))
        .route("/flows/{flow}/validate", get(validate_flow))
        .route("/flows/{flow}/agents", get(list_agents))
        .route("/flows/{flow}/events/{run_id}", get(flow_events))
        // Phase-1 Human-in-the-Loop conversation endpoints
        .route(
            "/flows/{flow}/conversations",
            get(conversations::list_conversations),
        )
        .route(
            "/flows/{flow}/conversations/{id}/start",
            post(conversations::start_conversation),
        )
        .route(
            "/flows/{flow}/conversations/{id}/messages",
            post(conversations::post_message),
        )
        .route(
            "/flows/{flow}/conversations/{id}/history",
            get(conversations::get_history),
        )
        .route(
            "/flows/{flow}/conversations/{id}/events",
            get(conversations::conversation_events),
        )
        .route(
            "/flows/{flow}/conversations/{id}",
            delete(conversations::delete_conversation),
        )
        .route("/nodes", get(list_nodes))
        .layer(axum::middleware::from_fn(auth::bearer_auth));

    public.merge(protected).with_state(state)
}
