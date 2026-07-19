pub mod admission;
pub mod audit;
pub mod auth;
pub mod conversations;
pub mod handlers;
pub mod idempotency;

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
use std::sync::atomic::{AtomicBool, AtomicUsize};
use tokio::sync::{RwLock, Semaphore};

use crate::engine::eventbus::EventBus;
use crate::engine::input_bridge::InputBridge;
use crate::engine::store::StateStore;

#[derive(Clone, Copy)]
pub struct CachedReadiness {
    pub checked_at: std::time::Instant,
    pub ready: bool,
    pub component: &'static str,
}

/// A running crew: its event bus and an abort handle to cancel it.
pub struct ActiveRun {
    pub eventbus: EventBus,
    pub abort_handle: tokio::task::AbortHandle,
    /// Flow slug this run belongs to, so `abort_run` can reject a request that
    /// targets another flow's run without a store round-trip.
    pub flow: String,
    /// Per-run human-input transport for `crew:ask_human()` — the questions
    /// and answer endpoints reach the suspended flow through this. Dropped
    /// with the entry, so pending oneshots die when the run is cleaned up.
    pub input_bridge: Arc<InputBridge>,
    /// Becomes `true` after the monitor has persisted and emitted exactly one
    /// terminal outcome. Shutdown waits on this after aborting active work.
    pub terminal: tokio::sync::watch::Receiver<bool>,
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
    /// Immutable authentication policy parsed once at startup. Successful
    /// protected requests receive a trusted `auth::Principal` extension.
    pub auth: Arc<auth::AuthConfig>,
    /// Principal-aware process admission and low-cardinality metrics.
    pub admission: Arc<admission::AdmissionController>,
    /// Cleared as soon as graceful shutdown begins so readiness fails before
    /// active work is cancelled and drained.
    pub accepting_traffic: AtomicBool,
    pub active_runs: Arc<RwLock<HashMap<String, ActiveRun>>>,
    pub active_conversations: ActiveConversationsMap,
    /// Hard cap on `active_conversations.len()` — reads
    /// `IRONCREW_MAX_ACTIVE_CONVERSATIONS` once at boot.
    pub max_active_conversations: usize,
    /// Atomic admission control for active conversations. Each live
    /// `ConversationHandle` owns one permit for its entire in-memory lifetime.
    pub conversation_permits: Arc<Semaphore>,
    /// Hard cap on `active_runs.len()` — reads `IRONCREW_MAX_ACTIVE_RUNS`
    /// once at boot. Backpressure against unbounded concurrent runs.
    pub max_active_runs: usize,
    /// Atomic admission control for active runs. The run monitor owns the
    /// permit until it has persisted and emitted the terminal outcome.
    pub run_permits: Arc<Semaphore>,
    /// Global admission control for long-lived run and conversation SSE
    /// connections. The stream owns a permit until the client disconnects.
    pub max_sse_connections: usize,
    pub sse_permits: Arc<Semaphore>,
    /// Hard lifetime for an HTTP run, captured once at server startup.
    pub max_run_lifetime: std::time::Duration,
    /// Number of HTTP run finalizers currently retrying a durable terminal
    /// transition. Any non-zero value fails readiness so ingress stops adding
    /// work while persistence is degraded.
    pub terminal_persistence_failures: AtomicUsize,
    /// Set by the lease heartbeat/reconciler loop. A failed maintenance write
    /// keeps readiness down until a complete heartbeat+reconcile cycle passes.
    pub store_maintenance_healthy: AtomicBool,
    /// Coalesces unauthenticated readiness probes and caches the expensive
    /// storage/schema result for a short interval.
    pub readiness_cache: tokio::sync::Mutex<Option<CachedReadiness>>,
    /// Immutable, boot-validated limits for the durable HTTP idempotency
    /// ledger. Keeping these in state avoids environment reads per request.
    pub idempotency: idempotency::IdempotencyConfig,
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
#[derive(Serialize, Deserialize)]
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

/// Build the router
pub fn create_router(state: Arc<AppState>) -> Router {
    use handlers::*;

    // Public routes (no auth required)
    let public = Router::new()
        .route("/health", get(health))
        .route("/health/live", get(health))
        .route("/health/ready", get(readiness));

    // Protected routes (auth required when either token source is configured).
    // Authentication is the outer layer: admission sees only server-issued
    // principal extensions, never X-Audit-Actor or source-IP guesses.
    let protected = Router::new()
        .route("/flows/{flow}/run", post(run_flow))
        .route("/flows/{flow}/abort/{run_id}", post(abort_run))
        .route("/flows/{flow}/runs", get(list_runs))
        .route("/flows/{flow}/runs/{id}", get(get_run))
        .route("/flows/{flow}/runs/{id}", delete(delete_run))
        .route("/flows/{flow}/validate", get(validate_flow))
        .route("/flows/{flow}/agents", get(list_agents))
        .route("/flows/{flow}/events/{run_id}", get(flow_events))
        // Mid-run Human-in-the-Loop (crew:ask_human) endpoints
        .route("/flows/{flow}/questions/{run_id}", get(list_questions))
        .route("/flows/{flow}/answer/{run_id}", post(answer_question))
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
        .route("/audit", get(handlers::list_audit))
        .route("/metrics", get(admission::metrics))
        .route("/nodes", get(list_nodes))
        .layer(axum::middleware::from_fn_with_state(
            state.admission.clone(),
            admission::enforce_mutation_admission,
        ))
        .layer(axum::middleware::from_fn_with_state(
            state.auth.clone(),
            auth::bearer_auth,
        ));

    public.merge(protected).with_state(state)
}
