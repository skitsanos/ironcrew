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

/// Shared application state
pub struct AppState {
    pub flows_dir: PathBuf,
    pub active_runs: Arc<RwLock<HashMap<String, EventBus>>>,
}

/// Response from running a crew
#[derive(Serialize)]
pub struct RunCrewResponse {
    pub run_id: String,
    pub flow_name: String,
    pub status: String,
    pub duration_ms: u64,
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

/// Query params for listing runs
#[derive(Deserialize)]
pub struct ListRunsQuery {
    pub status: Option<String>,
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

/// Resolve the runs directory for a given flow.
pub fn resolve_runs_dir(state: &AppState, flow: &str) -> crate::utils::error::Result<PathBuf> {
    Ok(resolve_flow_path(state, flow)?
        .join(".ironcrew")
        .join("runs"))
}

/// Build the router
pub fn create_router(state: Arc<AppState>) -> Router {
    use handlers::*;

    Router::new()
        .route("/health", get(health))
        .route("/flows/{flow}/run", post(run_flow))
        .route("/flows/{flow}/runs", get(list_runs))
        .route("/flows/{flow}/runs/{id}", get(get_run))
        .route("/flows/{flow}/runs/{id}", delete(delete_run))
        .route("/flows/{flow}/validate", get(validate_flow))
        .route("/flows/{flow}/agents", get(list_agents))
        .route("/flows/{flow}/events/{run_id}", get(flow_events))
        .route("/nodes", get(list_nodes))
        .with_state(state)
}
