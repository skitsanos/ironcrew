pub mod admission;
pub mod audit;
pub mod auth;
pub mod conversation_lifecycle;
pub mod conversations;
pub mod deployment;
pub mod handlers;
pub mod idempotency;
pub mod lifecycle;
mod resource_metrics;
mod sse;
mod state;

#[allow(unused_imports)] // public compatibility re-export; binary module tree is private
pub use state::{ActiveConversationsMap, ActiveRun, AppState, CachedReadiness};

use axum::{
    Router,
    http::StatusCode,
    response::Json,
    routing::{delete, get, post},
};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;

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

/// Run-event and human-input payloads (including validation errors) can carry
/// sensitive metadata. Apply this at the route boundary so extractor-generated
/// 4xx responses receive the same cache policy as successful handlers.
async fn sensitive_response_no_store(
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let mut response = next.run(request).await;
    response
        .headers_mut()
        .entry(axum::http::header::CACHE_CONTROL)
        .or_insert(axum::http::HeaderValue::from_static("no-store"));
    response
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

    let sensitive_run_control = Router::new()
        .route("/flows/{flow}/events/{run_id}", get(flow_events))
        .route("/flows/{flow}/questions/{run_id}", get(list_questions))
        .route("/flows/{flow}/answer/{run_id}", post(answer_question))
        .layer(axum::middleware::from_fn(sensitive_response_no_store));

    let sensitive_conversation_control = Router::new()
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
            "/flows/{flow}/conversations/{id}",
            delete(conversations::delete_conversation),
        )
        .route(
            "/flows/{flow}/conversations/{id}/events",
            get(conversations::conversation_events),
        )
        .layer(axum::middleware::from_fn(sensitive_response_no_store));

    // Protected routes (auth required when either token source is configured).
    // Authentication is the outer layer: admission sees only server-issued
    // principal extensions, never X-Audit-Actor or source-IP guesses.
    let protected = Router::new()
        .route("/capabilities", get(capabilities))
        .route("/flows/{flow}/run", post(run_flow))
        .route("/flows/{flow}/abort/{run_id}", post(abort_run))
        .route("/flows/{flow}/runs", get(list_runs))
        .route("/flows/{flow}/runs/{id}", get(get_run))
        .route("/flows/{flow}/runs/{id}", delete(delete_run))
        .route("/flows/{flow}/validate", get(validate_flow))
        .route("/flows/{flow}/agents", get(list_agents))
        // Mid-run Human-in-the-Loop (crew:ask_human) endpoints. Their route
        // layer also marks extractor-generated errors as non-cacheable.
        .merge(sensitive_run_control)
        // Conversation payloads and all extractor/error responses are
        // non-cacheable because transcripts and model output are sensitive.
        .merge(sensitive_conversation_control)
        .route("/audit", get(handlers::list_audit))
        .route("/metrics", get(admission::metrics))
        .route("/nodes", get(list_nodes))
        .layer(axum::middleware::from_fn_with_state(
            state.admission.clone(),
            admission::enforce_mutation_admission,
        ))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            lifecycle::enforce_mutation_lifecycle,
        ))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            lifecycle::attach_instance_id,
        ))
        .layer(axum::middleware::from_fn_with_state(
            state.auth.clone(),
            auth::bearer_auth,
        ));

    public.merge(protected).with_state(state)
}
