use axum::{
    extract::{ConnectInfo, Extension, Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::Json,
    response::sse::{Event, Sse},
};
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast;

use crate::engine::eventbus::{CrewEvent, EventBus};
use crate::engine::idempotency::{
    IdempotencyClaim, IdempotencyClaimOutcome, IdempotencyCompletion, IdempotencyLookup,
    IdempotencyQuotaResource, IdempotencyQuotaScope, IdempotencyRecord, PrincipalId, RUN_OPERATION,
    RunFenceHeartbeat, RunIntentSignal,
};
use crate::engine::run_history::{RunCompletion, RunIntent, RunStatus, RunTransition};
use crate::engine::store::create_store;
use crate::utils::error::IronCrewError;

use super::admission::QuotaMetric;
use super::auth::Principal;
use super::{
    AppState, ErrorResponse, ListRunsQuery, ListRunsResponse, RunCrewResponse, TaskResultResponse,
    error_response, resolve_flow_path,
};

#[derive(Clone)]
struct RunIdempotencyAttempt {
    key_hash: String,
    principal_id: PrincipalId,
    request_fingerprint: String,
    attempt_id: String,
    response_body: String,
}

type RunFlowResult =
    Result<(HeaderMap, Json<serde_json::Value>), (StatusCode, Json<ErrorResponse>)>;

fn replay_run(record: &IdempotencyRecord) -> RunFlowResult {
    let (Some(status), Some(body)) = (record.response_status, record.response_body.as_deref())
    else {
        return Err(error_response(
            StatusCode::CONFLICT,
            "The prior request cannot be replayed; use a new Idempotency-Key after verifying its outcome"
                .into(),
        ));
    };
    if status != StatusCode::OK.as_u16() {
        return Err(error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Stored idempotency response has an invalid status".into(),
        ));
    }
    let body = serde_json::from_str(body).map_err(|error| {
        tracing::error!(%error, "Stored run idempotency response is corrupt");
        error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Stored idempotency response is corrupt".into(),
        )
    })?;
    Ok((super::idempotency::replay_headers(), Json(body)))
}

fn idempotency_store_error(error: IronCrewError) -> (StatusCode, Json<ErrorResponse>) {
    tracing::error!(%error, "Idempotency storage operation failed");
    error_response(
        StatusCode::SERVICE_UNAVAILABLE,
        "Idempotency storage is temporarily unavailable".into(),
    )
}

fn idempotency_quota_error(
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

async fn release_run_idempotency(state: &AppState, attempt: Option<&RunIdempotencyAttempt>) {
    let Some(attempt) = attempt else {
        return;
    };
    if let Err(error) = state
        .store
        .release_idempotency(&attempt.key_hash, &attempt.attempt_id)
        .await
    {
        tracing::error!(%error, "Failed to release an unstarted run idempotency claim");
    }
}

async fn complete_run_idempotency(state: &Arc<AppState>, attempt: RunIdempotencyAttempt) {
    let completed_at = chrono::Utc::now();
    let completion = IdempotencyCompletion {
        key_hash: attempt.key_hash,
        principal_id: attempt.principal_id,
        request_fingerprint: attempt.request_fingerprint,
        attempt_id: attempt.attempt_id,
        owner_instance_id: state.store.instance_id().to_string(),
        response_status: StatusCode::OK.as_u16(),
        response_body: Some(attempt.response_body),
        completed_at: completed_at.to_rfc3339(),
        expires_at: state.idempotency.retention_expiry(completed_at),
    };
    let mut persistence_degraded = false;
    let mut retry_delay = std::time::Duration::from_millis(250);
    loop {
        match state
            .store
            .complete_idempotency_with_limits(completion.clone(), state.idempotency.limits())
            .await
        {
            Ok(_) => {
                if persistence_degraded {
                    state
                        .terminal_persistence_failures
                        .fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
                }
                return;
            }
            Err(error @ IronCrewError::Conflict(_)) => {
                tracing::warn!(%error, "Run idempotency completion was fenced");
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
                    "Run idempotency completion failed; retaining admission and retrying"
                );
                tokio::time::sleep(retry_delay).await;
                retry_delay = retry_delay
                    .saturating_mul(2)
                    .min(std::time::Duration::from_secs(30));
            }
        }
    }
}

async fn mark_run_idempotency_indeterminate(
    state: &Arc<AppState>,
    attempt: &RunIdempotencyAttempt,
) {
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
            Ok(_) => {
                if persistence_degraded {
                    state
                        .terminal_persistence_failures
                        .fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
                }
                return;
            }
            Err(error @ IronCrewError::Conflict(_)) => {
                tracing::warn!(%error, "Indeterminate run finalization was fenced");
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
                    "Failed to preserve an indeterminate run outcome; retrying"
                );
                tokio::time::sleep(retry_delay).await;
                retry_delay = retry_delay.saturating_mul(2).min(Duration::from_secs(30));
            }
        }
    }
}

const HARD_API_MAX_TAGS: usize = 256;
const HARD_API_MAX_TAG_BYTES: usize = 4 * 1024;
const HARD_API_MAX_TAGS_BYTES: usize = 64 * 1024;

fn validate_run_id(run_id: &str) -> Result<(), IronCrewError> {
    if run_id.is_empty()
        || run_id.len() > 128
        || !run_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err(IronCrewError::Validation(
            "Invalid run identifier; expected 1-128 ASCII letters, digits, '.', '_' or '-'".into(),
        ));
    }
    Ok(())
}

fn validate_run_tags(input: Option<&serde_json::Value>) -> Result<Vec<String>, IronCrewError> {
    let Some(tags_value) = input.and_then(|value| value.get("tags")) else {
        return Ok(Vec::new());
    };
    let tags = tags_value.as_array().ok_or_else(|| {
        IronCrewError::Validation("Input 'tags' must be an array of strings".into())
    })?;
    let max_tags = positive_bounded_env("IRONCREW_API_MAX_TAGS", 32, HARD_API_MAX_TAGS);
    if tags.len() > max_tags {
        return Err(IronCrewError::Validation(format!(
            "Input has {} tags, exceeds IRONCREW_API_MAX_TAGS ({max_tags})",
            tags.len()
        )));
    }

    let max_tag_bytes =
        positive_bounded_env("IRONCREW_API_MAX_TAG_BYTES", 256, HARD_API_MAX_TAG_BYTES);
    let max_total_bytes = positive_bounded_env(
        "IRONCREW_API_MAX_TAGS_BYTES",
        4 * 1024,
        HARD_API_MAX_TAGS_BYTES,
    );
    let mut total_bytes = 0usize;
    let mut validated = Vec::with_capacity(tags.len());
    let mut seen = std::collections::HashSet::with_capacity(tags.len());
    for (index, value) in tags.iter().enumerate() {
        let tag = value.as_str().ok_or_else(|| {
            IronCrewError::Validation(format!("Input tags[{index}] must be a string"))
        })?;
        if tag.is_empty() || tag.trim() != tag || tag.chars().any(char::is_control) {
            return Err(IronCrewError::Validation(format!(
                "Input tags[{index}] must be non-empty and contain no padding or control characters"
            )));
        }
        if tag.len() > max_tag_bytes {
            return Err(IronCrewError::Validation(format!(
                "Input tags[{index}] is {} bytes, exceeds IRONCREW_API_MAX_TAG_BYTES ({max_tag_bytes})",
                tag.len()
            )));
        }
        total_bytes = total_bytes
            .checked_add(tag.len())
            .ok_or_else(|| IronCrewError::Validation("Input tag byte count overflowed".into()))?;
        if total_bytes > max_total_bytes {
            return Err(IronCrewError::Validation(format!(
                "Input tags total {total_bytes} bytes, exceeds IRONCREW_API_MAX_TAGS_BYTES ({max_total_bytes})"
            )));
        }
        if !seen.insert(tag) {
            return Err(IronCrewError::Validation(format!(
                "Input tags contains duplicate '{tag}'"
            )));
        }
        validated.push(tag.to_string());
    }
    Ok(validated)
}

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

/// Readiness probe: unlike the compatibility `/health` liveness endpoint,
/// this verifies both the configured flows directory and the persistence
/// backend before allowing the pod to receive traffic.
pub async fn readiness(
    State(state): State<Arc<AppState>>,
) -> (StatusCode, Json<serde_json::Value>) {
    if !state
        .accepting_traffic
        .load(std::sync::atomic::Ordering::Acquire)
    {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({
                "status": "not_ready",
                "component": "shutdown",
                "version": env!("CARGO_PKG_VERSION"),
            })),
        );
    }

    if state
        .terminal_persistence_failures
        .load(std::sync::atomic::Ordering::Acquire)
        > 0
    {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({
                "status": "not_ready",
                "component": "storage_finalization",
                "version": env!("CARGO_PKG_VERSION"),
            })),
        );
    }

    if !state
        .store_maintenance_healthy
        .load(std::sync::atomic::Ordering::Acquire)
    {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({
                "status": "not_ready",
                "component": "storage_maintenance",
                "version": env!("CARGO_PKG_VERSION"),
            })),
        );
    }

    let mut cache = match state.readiness_cache.try_lock() {
        Ok(cache) => cache,
        Err(_) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({
                    "status": "not_ready",
                    "component": "readiness_check",
                    "version": env!("CARGO_PKG_VERSION"),
                })),
            );
        }
    };
    let cache_ttl = std::time::Duration::from_millis(positive_bounded_env(
        "IRONCREW_READINESS_CACHE_MS",
        1_000,
        10_000,
    ) as u64);
    if let Some(snapshot) = *cache
        && snapshot.checked_at.elapsed() < cache_ttl
    {
        let status = if snapshot.ready {
            StatusCode::OK
        } else {
            StatusCode::SERVICE_UNAVAILABLE
        };
        return (
            status,
            Json(serde_json::json!({
                "status": if snapshot.ready { "ready" } else { "not_ready" },
                "component": snapshot.component,
                "version": env!("CARGO_PKG_VERSION"),
            })),
        );
    }

    let snapshot = if let Err(error) = tokio::fs::read_dir(&state.flows_dir).await {
        tracing::warn!(
            path = %state.flows_dir.display(),
            %error,
            "Readiness check failed: flows directory is unavailable"
        );
        super::CachedReadiness {
            checked_at: std::time::Instant::now(),
            ready: false,
            component: "flows",
        }
    } else if let Err(error) = state.store.health_check().await {
        tracing::warn!(%error, "Readiness check failed: state store is unavailable");
        super::CachedReadiness {
            checked_at: std::time::Instant::now(),
            ready: false,
            component: "storage",
        }
    } else {
        super::CachedReadiness {
            checked_at: std::time::Instant::now(),
            ready: true,
            component: "storage",
        }
    };
    *cache = Some(snapshot);

    let status = if snapshot.ready {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (
        status,
        Json(serde_json::json!({
            "status": if snapshot.ready { "ready" } else { "not_ready" },
            "component": snapshot.component,
            "version": env!("CARGO_PKG_VERSION"),
        })),
    )
}

fn run_not_found(error: &IronCrewError, run_id: &str) -> bool {
    matches!(
        error,
        IronCrewError::Validation(message)
            if message == &format!("Run '{}' not found", run_id)
    )
}

/// Persist one terminal transition for an HTTP-owned run. Normal crew runs
/// have already completed their record inside `crew:run()`; in that case the
/// store returns `AlreadyTerminal` and its winning status is preserved. If a
/// task failed before Lua could create the intent, create a minimal fallback
/// record only after confirming the run is genuinely absent.
struct TerminalPersistence<'a> {
    run_id: &'a str,
    flow: &'a str,
    started_at: &'a str,
    tags: &'a [String],
    status: RunStatus,
    duration_ms: u64,
    total_tokens: u32,
}

async fn persist_terminal_outcome(
    store: &Arc<dyn crate::engine::store::StateStore>,
    terminal: TerminalPersistence<'_>,
) -> Result<RunStatus, IronCrewError> {
    let completion = RunCompletion {
        status: terminal.status.clone(),
        finished_at: chrono::Utc::now().to_rfc3339(),
        duration_ms: terminal.duration_ms,
        task_results: Vec::new(),
        total_tokens: terminal.total_tokens,
        cached_tokens: 0,
    };

    match store
        .update_run_completion(terminal.run_id, completion.clone())
        .await
    {
        Ok(RunTransition::Applied) => return Ok(terminal.status),
        Ok(RunTransition::AlreadyTerminal(status)) => return Ok(status),
        Err(update_error) => match store.get_run(terminal.run_id).await {
            Ok(record) if record.status.is_terminal() => return Ok(record.status),
            Ok(record) => {
                return Err(IronCrewError::Validation(format!(
                    "Failed to persist terminal outcome for run '{}': {}; durable status remains '{}'",
                    terminal.run_id, update_error, record.status
                )));
            }
            Err(get_error) if run_not_found(&get_error, terminal.run_id) => {}
            Err(get_error) => {
                return Err(IronCrewError::Validation(format!(
                    "Could not verify run '{}' after terminal update failed (update: {}; read: {})",
                    terminal.run_id, update_error, get_error
                )));
            }
        },
    }

    let fallback = RunIntent {
        suggested_id: Some(terminal.run_id.to_string()),
        flow_name: terminal.flow.to_string(),
        flow: terminal.flow.to_string(),
        started_at: terminal.started_at.to_string(),
        agent_count: 0,
        task_count: 0,
        tags: terminal.tags.to_vec(),
    };
    if let Err(error) = store.save_run_intent(fallback).await {
        return Err(IronCrewError::Validation(format!(
            "Failed to create fallback intent for terminal run '{}': {}",
            terminal.run_id, error
        )));
    }

    match store
        .update_run_completion(terminal.run_id, completion)
        .await
    {
        Ok(RunTransition::Applied) => Ok(terminal.status),
        Ok(RunTransition::AlreadyTerminal(status)) => Ok(status),
        Err(error) => Err(IronCrewError::Validation(format!(
            "Failed to persist terminal outcome for fallback run '{}': {}",
            terminal.run_id, error
        ))),
    }
}

struct WorkOutcome {
    status: RunStatus,
    duration_ms: u64,
    total_tokens: u32,
    error_message: Option<String>,
}

/// Tokio detaches a task when its `JoinHandle` is dropped. The run monitor is
/// the task's safety owner, so a monitor panic/cancellation must abort Lua
/// instead of leaving untracked provider or tool calls running.
struct AbortTaskOnDrop<T>(tokio::task::JoinHandle<T>);

impl<T> AbortTaskOnDrop<T> {
    fn new(handle: tokio::task::JoinHandle<T>) -> Self {
        Self(handle)
    }

    fn handle_mut(&mut self) -> &mut tokio::task::JoinHandle<T> {
        &mut self.0
    }

    fn abort(&self) {
        self.0.abort();
    }
}

impl<T> Drop for AbortTaskOnDrop<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

fn classify_work_result(
    join_result: std::result::Result<
        std::result::Result<RunCrewResponse, IronCrewError>,
        tokio::task::JoinError,
    >,
    elapsed_ms: u64,
) -> WorkOutcome {
    match join_result {
        Ok(Ok(response)) => WorkOutcome {
            status: response
                .status
                .parse::<RunStatus>()
                .ok()
                .filter(RunStatus::is_terminal)
                .unwrap_or(RunStatus::Success),
            duration_ms: response.duration_ms,
            total_tokens: response.total_tokens,
            error_message: None,
        },
        Ok(Err(error)) => WorkOutcome {
            status: RunStatus::Failed,
            duration_ms: elapsed_ms,
            total_tokens: 0,
            error_message: Some(error.to_string()),
        },
        Err(join_error) if join_error.is_cancelled() => WorkOutcome {
            status: RunStatus::Aborted,
            duration_ms: elapsed_ms,
            total_tokens: 0,
            error_message: None,
        },
        Err(join_error) => WorkOutcome {
            status: RunStatus::Failed,
            duration_ms: elapsed_ms,
            total_tokens: 0,
            error_message: Some(format!("Task panicked: {join_error}")),
        },
    }
}

// ---------------------------------------------------------------------------
// Flow execution
// ---------------------------------------------------------------------------

pub async fn run_flow(
    State(state): State<Arc<AppState>>,
    Extension(principal): Extension<Principal>,
    Path(flow): Path<String>,
    headers: HeaderMap,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    body: Option<Json<serde_json::Value>>,
) -> RunFlowResult {
    let input = body.map(|Json(v)| v);
    let request_key =
        super::idempotency::request_key(&headers, state.idempotency.require_key, principal.id())
            .map_err(|error| error_response(StatusCode::BAD_REQUEST, error.to_string()))?;
    let request_fingerprint = super::idempotency::run_fingerprint(&flow, input.as_ref());

    // Look up before flow resolution and admission so a completed request can
    // still be recovered after a rolling restart (or after the flow source is
    // temporarily unavailable). Only digests, never the raw client key, reach
    // the persistence layer or logs.
    if let Some(key) = request_key.as_ref() {
        let now = chrono::Utc::now().to_rfc3339();
        match state
            .store
            .lookup_idempotency_for_principal(
                principal.id(),
                &key.key_hash,
                &request_fingerprint,
                &now,
            )
            .await
            .map_err(idempotency_store_error)?
        {
            IdempotencyLookup::Miss => {}
            IdempotencyLookup::Replay(record) => return replay_run(&record),
            IdempotencyLookup::InProgress(_) => {
                return Err(error_response(
                    StatusCode::CONFLICT,
                    "This idempotent run is not durably accepted yet; retry shortly".into(),
                ));
            }
            IdempotencyLookup::Indeterminate(_) => {
                return Err(error_response(
                    StatusCode::CONFLICT,
                    "The prior request has an indeterminate outcome; inspect its run before using a new Idempotency-Key"
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

    // Extract tags up front so the audit metadata can be populated even
    // when flow resolution fails. Tags are user-controlled and bounded by
    // the audit recorder's metadata clamp.
    let tags_for_audit = validate_run_tags(input.as_ref())
        .map_err(|error| error_response(StatusCode::BAD_REQUEST, sanitize_error(&error)))?;

    let flow_path = match resolve_flow_path(&state, &flow) {
        Ok(p) => p,
        Err(e) => {
            let resp = error_response(flow_status(&e), sanitize_error(&e));
            let status_code = resp.0.as_u16();
            let metadata = if !tags_for_audit.is_empty() {
                Some(serde_json::json!({ "tags": tags_for_audit }))
            } else {
                None
            };
            crate::api::audit::record(
                &state.store,
                "flow.run.start",
                Some(&flow),
                None,
                &headers,
                Some(addr),
                false,
                status_code,
                metadata,
            )
            .await;
            return Err(resp);
        }
    };

    if !state
        .accepting_traffic
        .load(std::sync::atomic::Ordering::Acquire)
    {
        return Err(error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "Server is shutting down".into(),
        ));
    }

    // Reserve capacity atomically. A `len()` pre-check races concurrent HTTP
    // requests and can oversubscribe the process before any run reaches the
    // active map. The monitor owns this permit through terminal persistence.
    let admission_permit = state
        .run_permits
        .clone()
        .try_acquire_owned()
        .map_err(|_| {
            error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                format!(
                    "Active run limit reached ({} runs). Raise IRONCREW_MAX_ACTIVE_RUNS or wait for in-flight runs to finish.",
                    state.max_active_runs
                ),
            )
        })?;

    // Flow slug the run will be scoped by — the resolved directory's last
    // segment, matching what `crew:run()` stores in `RunRecord::flow`.
    let flow_slug = flow_path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_string();

    let run_id = uuid::Uuid::new_v4().to_string();
    let response_value = serde_json::json!({
        "run_id": run_id,
        "status": "started",
        "events_url": format!("/flows/{}/events/{}", flow, run_id),
    });

    // The acceptance response and its run id are claimed durably before Lua
    // starts. Concurrent pods therefore converge on one run id and never
    // launch duplicate work for the same key.
    let idempotency_attempt = if let Some(key) = request_key.as_ref() {
        let response_body = super::idempotency::bounded_response_json(
            &response_value,
            state.idempotency.max_response_bytes,
        )
        .map_err(idempotency_store_error)?
        .ok_or_else(|| {
            error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Run acceptance response exceeded the idempotency response limit".into(),
            )
        })?;
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
            key_hash: key.key_hash.clone(),
            principal_id: principal.id().clone(),
            recovery_key_hash: None,
            request_fingerprint: request_fingerprint.clone(),
            operation: RUN_OPERATION.into(),
            scope: super::idempotency::run_scope(&flow_slug),
            resource_id: run_id.clone(),
            exclusive_scope: None,
            attempt_id: attempt_id.clone(),
            owner_instance_id: state.store.instance_id().to_string(),
            base_revision: None,
            response_status: Some(StatusCode::OK.as_u16()),
            response_body: Some(response_body.clone()),
            max_total_response_bytes: state.idempotency.max_total_response_bytes,
            lease_expires_at,
            created_at: now.to_rfc3339(),
            ttl_seconds: state.idempotency.ttl_seconds,
        };
        match state
            .store
            .claim_idempotency_with_limits(claim, state.idempotency.limits())
            .await
            .map_err(idempotency_store_error)?
        {
            IdempotencyClaimOutcome::Claimed(_) => Some(RunIdempotencyAttempt {
                key_hash: key.key_hash.clone(),
                principal_id: principal.id().clone(),
                request_fingerprint: request_fingerprint.clone(),
                attempt_id,
                response_body,
            }),
            IdempotencyClaimOutcome::Replay(record) => return replay_run(&record),
            IdempotencyClaimOutcome::InProgress(_) => {
                return Err(error_response(
                    StatusCode::CONFLICT,
                    "This idempotent run is not durably accepted yet; retry shortly".into(),
                ));
            }
            IdempotencyClaimOutcome::Indeterminate(_) => {
                return Err(error_response(
                    StatusCode::CONFLICT,
                    "The prior request has an indeterminate outcome; inspect its run before using a new Idempotency-Key"
                        .into(),
                ));
            }
            IdempotencyClaimOutcome::Conflict => {
                return Err(error_response(
                    StatusCode::CONFLICT,
                    "Idempotency-Key was already used for a different request".into(),
                ));
            }
            IdempotencyClaimOutcome::Busy => {
                return Err(error_response(
                    StatusCode::CONFLICT,
                    "An idempotent operation is already in progress".into(),
                ));
            }
            IdempotencyClaimOutcome::QuotaExceeded {
                scope,
                resource,
                retry_after_seconds,
            } => {
                return Err(idempotency_quota_error(
                    &state,
                    scope,
                    resource,
                    retry_after_seconds,
                ));
            }
        }
    } else {
        None
    };
    let eventbus = EventBus::new(256);
    let started_at = chrono::Utc::now().to_rfc3339();
    let started = std::time::Instant::now();

    // Per-run human-input transport: crew:ask_human() parks on this, the
    // questions/answer endpoints reach it through ActiveRun.
    let input_bridge = Arc::new(crate::engine::input_bridge::InputBridge::new(
        crate::engine::input_bridge::BridgeMode::Http,
    ));

    // Prepare the work task, then register it while holding the active-map
    // write lock. Rechecking readiness under that lock closes the race where
    // shutdown drains the map while a request is still being initialized.
    let eventbus_inner = eventbus.clone();
    let run_id_for_work = run_id.clone();
    let store_for_work = state.store.clone();
    let bridge_for_work = input_bridge.clone();
    let (run_intent_signal, mut run_intent_ready) =
        if let Some(attempt) = idempotency_attempt.as_ref() {
            let (signal, receiver) = RunIntentSignal::channel(
                attempt.key_hash.clone(),
                attempt.principal_id.clone(),
                attempt.request_fingerprint.clone(),
                attempt.attempt_id.clone(),
            );
            (Some(signal), Some(receiver))
        } else {
            (None, None)
        };
    let (work_handle, terminal_tx) = {
        let mut active_runs = state.active_runs.write().await;
        if !state
            .accepting_traffic
            .load(std::sync::atomic::Ordering::Acquire)
        {
            drop(active_runs);
            release_run_idempotency(&state, idempotency_attempt.as_ref()).await;
            return Err(error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                "Server is shutting down".into(),
            ));
        }

        // Terminal entries are retained briefly so late SSE subscribers can
        // recover the final event. Reclaim them before admitting new work so
        // rapid completions can never make the event-bus map exceed the same
        // bound used for active-run admission.
        active_runs.retain(|_, active| !*active.terminal.borrow());
        if active_runs.len() >= state.max_active_runs {
            drop(active_runs);
            release_run_idempotency(&state, idempotency_attempt.as_ref()).await;
            return Err(error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                "Active run registry is at capacity; retry shortly".into(),
            ));
        }

        let work_handle = tokio::spawn(async move {
            execute_crew_from_path_with_events(
                &flow_path,
                &eventbus_inner,
                &run_id_for_work,
                input.as_ref(),
                Some(store_for_work),
                Some(bridge_for_work),
                run_intent_signal,
            )
            .await
        });
        let abort_handle = work_handle.abort_handle();
        let (terminal_tx, terminal_rx) = tokio::sync::watch::channel(false);

        active_runs.insert(
            run_id.clone(),
            super::ActiveRun {
                eventbus: eventbus.clone(),
                abort_handle,
                flow: flow_slug.clone(),
                input_bridge,
                terminal: terminal_rx,
            },
        );
        (work_handle, terminal_tx)
    };

    let run_id_clone = run_id.clone();
    let state_clone = state.clone();
    let flow_slug_for_monitor = flow_slug.clone();
    let tags_for_terminal = tags_for_audit.clone();
    let idempotency_for_monitor = idempotency_attempt.clone();

    // The request future may be cancelled as soon as the client disconnects,
    // but keyed work deliberately continues under server ownership. Give the
    // start audit the same detached ownership so every accepted durable run
    // has a corresponding event. No request body or raw idempotency key is
    // included in audit metadata.
    let mut audit_ready = run_intent_ready.clone();
    let audit_store = state.store.clone();
    let audit_flow = flow.clone();
    let audit_run_id = run_id.clone();
    let audit_headers = crate::api::audit::background_headers(&headers);
    let audit_metadata = if tags_for_audit.is_empty() {
        None
    } else {
        Some(serde_json::json!({ "tags": tags_for_audit }))
    };
    tokio::spawn(async move {
        let accepted = match audit_ready.as_mut() {
            Some(ready) => ready.wait_for(|is_ready| *is_ready).await.is_ok(),
            None => true,
        };
        crate::api::audit::record(
            &audit_store,
            "flow.run.start",
            Some(&audit_flow),
            Some(&audit_run_id),
            &audit_headers,
            Some(addr),
            accepted,
            if accepted { 200 } else { 503 },
            audit_metadata,
        )
        .await;
    });

    // Monitor the work handle. It is the single API-level finalizer for
    // errors, cancellation, panic, timeout, and server shutdown. Store
    // transitions are compare-and-set, so a normal `crew:run()` completion
    // that wins first remains authoritative.
    tokio::spawn(async move {
        let max_lifetime = state_clone.max_run_lifetime;
        let mut work_handle = AbortTaskOnDrop::new(work_handle);

        // Keep the operation claim live independently of the client
        // connection. Losing the fence actively aborts Lua: continuing tools
        // or provider calls after another attempt owns the claim is unsafe.
        let run_heartbeat = idempotency_for_monitor.as_ref().map(|attempt| {
            super::idempotency::RunLeaseHeartbeat::spawn(
                state_clone.store.clone(),
                run_id_clone.clone(),
                attempt.key_hash.clone(),
                attempt.attempt_id.clone(),
            )
        });
        let mut fence_outcome = run_heartbeat
            .as_ref()
            .map(super::idempotency::RunLeaseHeartbeat::outcome_receiver);

        let (requested_status, duration_ms, total_tokens, error_message, fence_result) = tokio::select! {
            join_result = work_handle.handle_mut() => {
                let outcome = classify_work_result(
                    join_result,
                    started.elapsed().as_millis() as u64,
                );
                (
                    outcome.status,
                    outcome.duration_ms,
                    outcome.total_tokens,
                    outcome.error_message,
                    None,
                )
            }
            _ = tokio::time::sleep(max_lifetime) => {
                work_handle.abort();
                // Wait until cancellation has completed before touching the
                // record, so Lua cannot race a later completion write.
                let _ = work_handle.handle_mut().await;
                tracing::warn!("Run {} timed out after {}s", run_id_clone, max_lifetime.as_secs());
                (
                    RunStatus::TimedOut,
                    started.elapsed().as_millis() as u64,
                    0,
                    None,
                    None,
                )
            }
            outcome = async {
                match fence_outcome.as_mut() {
                    Some(outcome) => super::idempotency::wait_for_run_fence_outcome(outcome).await,
                    None => std::future::pending::<RunFenceHeartbeat>().await,
                }
            } => {
                work_handle.abort();
                let _ = work_handle.handle_mut().await;
                match outcome {
                    RunFenceHeartbeat::Terminal(status) => {
                        tracing::debug!(run_id = %run_id_clone, %status, "Run worker stopped after its durable record became terminal");
                        let terminal_result = RunFenceHeartbeat::Terminal(status.clone());
                        let (duration_ms, total_tokens) = match state_clone
                            .store
                            .get_run(&run_id_clone)
                            .await
                        {
                            Ok(record) if record.status.is_terminal() => {
                                (record.duration_ms, record.total_tokens)
                            }
                            Ok(record) => {
                                tracing::warn!(
                                    run_id = %run_id_clone,
                                    durable_status = %record.status,
                                    "Run-fence heartbeat reported a terminal status but the follow-up read was in-flight"
                                );
                                (started.elapsed().as_millis() as u64, 0)
                            }
                            Err(error) => {
                                tracing::error!(
                                    run_id = %run_id_clone,
                                    %error,
                                    "Failed to read terminal run metrics after a run-fence heartbeat"
                                );
                                (started.elapsed().as_millis() as u64, 0)
                            }
                        };
                        (
                            status,
                            duration_ms,
                            total_tokens,
                            None,
                            Some(terminal_result),
                        )
                    }
                    RunFenceHeartbeat::Lost | RunFenceHeartbeat::Owned => {
                        tracing::warn!(run_id = %run_id_clone, "Run stopped after losing its durable execution fence");
                        (
                            RunStatus::Abandoned,
                            started.elapsed().as_millis() as u64,
                            0,
                            Some("Run stopped after its durable execution fence was lost".into()),
                            Some(RunFenceHeartbeat::Lost),
                        )
                    }
                }
            }
        };
        if let Some(message) = error_message {
            eventbus.emit(CrewEvent::Log {
                level: "error".into(),
                message,
            });
        }

        let mut persistence_degraded = false;
        let mut retry_delay = std::time::Duration::from_millis(250);
        let terminal_status = loop {
            match persist_terminal_outcome(
                &state_clone.store,
                TerminalPersistence {
                    run_id: &run_id_clone,
                    flow: &flow_slug_for_monitor,
                    started_at: &started_at,
                    tags: &tags_for_terminal,
                    status: requested_status.clone(),
                    duration_ms,
                    total_tokens,
                },
            )
            .await
            {
                Ok(status) => {
                    if persistence_degraded {
                        state_clone
                            .terminal_persistence_failures
                            .fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
                        tracing::info!(run_id = %run_id_clone, "Terminal persistence recovered");
                    }
                    break status;
                }
                Err(error) => {
                    if !persistence_degraded {
                        persistence_degraded = true;
                        state_clone
                            .terminal_persistence_failures
                            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
                    }
                    tracing::error!(
                        run_id = %run_id_clone,
                        retry_ms = retry_delay.as_millis(),
                        %error,
                        "Terminal persistence failed; retaining admission permit and retrying"
                    );
                    tokio::time::sleep(retry_delay).await;
                    retry_delay = retry_delay
                        .saturating_mul(2)
                        .min(std::time::Duration::from_secs(30));
                }
            }
        };

        if let Some(attempt) = idempotency_for_monitor {
            if matches!(fence_result, Some(RunFenceHeartbeat::Lost)) {
                mark_run_idempotency_indeterminate(&state_clone, &attempt).await;
            } else {
                complete_run_idempotency(&state_clone, attempt).await;
            }
        }
        drop(run_heartbeat);

        eventbus.emit(CrewEvent::RunComplete {
            run_id: run_id_clone.clone(),
            status: terminal_status.to_string(),
            duration_ms,
            total_tokens,
        });
        let _ = terminal_tx.send(true);
        drop(admission_permit);

        // Keep the terminal bus for late SSE recovery. Admission prunes these
        // tombstones early when capacity is needed, so retention is bounded by
        // `max_active_runs` rather than completion rate.
        tokio::time::sleep(run_sse_retention()).await;
        state_clone.active_runs.write().await.remove(&run_id_clone);
    });

    // A claimed row deliberately contains the eventual response but is not
    // replayable. Wait until `crew:run()` has persisted the matching run
    // intent and advanced the claim to `running` before acknowledging it.
    // The monitor owns execution, admission, and finalization, so dropping
    // this client connection cannot orphan the task while we wait.
    if let Some(ready) = run_intent_ready.as_mut()
        && ready.wait_for(|is_ready| *is_ready).await.is_err()
    {
        return Err(error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "Run failed before its acceptance could be persisted; retry with the same Idempotency-Key"
                .into(),
        ));
    }

    let response = Json(response_value);

    Ok((HeaderMap::new(), response))
}

/// Execute a crew from a flow path, injecting an EventBus, run_id, and optional input context.
async fn execute_crew_from_path_with_events(
    flow_path: &std::path::Path,
    eventbus: &EventBus,
    run_id: &str,
    input: Option<&serde_json::Value>,
    shared_store: Option<Arc<dyn crate::engine::store::StateStore>>,
    input_bridge: Option<Arc<crate::engine::input_bridge::InputBridge>>,
    run_intent_signal: Option<RunIntentSignal>,
) -> std::result::Result<RunCrewResponse, IronCrewError> {
    use crate::cli::project::{load_project, setup_crew_runtime};
    use crate::lua::api::json_value_to_lua;

    // A Lua entrypoint may perform asynchronous setup (including
    // `ask_human`) before it reaches `crew:run()`. For keyed HTTP work, create
    // the provisional run intent first so neither the initial 200 nor a replay
    // can ever reference a run that is still only process-local.
    if let Some(signal) = run_intent_signal.as_ref() {
        let store = shared_store.as_ref().ok_or_else(|| {
            IronCrewError::Validation(
                "A keyed HTTP run requires the server's shared state store".into(),
            )
        })?;
        let flow_slug = flow_path
            .file_name()
            .and_then(|segment| segment.to_str())
            .unwrap_or("")
            .to_string();
        let started_at = chrono::Utc::now().to_rfc3339();
        store
            .save_run_intent(RunIntent {
                suggested_id: Some(run_id.to_string()),
                flow_name: flow_slug.clone(),
                flow: flow_slug,
                started_at: started_at.clone(),
                agent_count: 0,
                task_count: 0,
                tags: validate_run_tags(input)?,
            })
            .await?;

        let (principal_id, key_hash, request_fingerprint) = signal.lookup_identity();
        let lookup = store
            .lookup_idempotency_for_principal(
                principal_id,
                key_hash,
                request_fingerprint,
                &started_at,
            )
            .await?;
        match lookup {
            IdempotencyLookup::Replay(record) if signal.matches_running(&record, run_id) => {
                signal.notify();
            }
            _ => {
                return Err(IronCrewError::Conflict(
                    "Run idempotency claim was not linked to its durable run intent".into(),
                ));
            }
        }
    }

    let loader = load_project(flow_path)?;
    let (lua, _runtime) = setup_crew_runtime(&loader)?;

    // Store the eventbus in a Lua global so LuaCrew::run() can pick it up
    lua.set_app_data(eventbus.clone());

    // Store the run_id so LuaCrew::run() uses it for the RunRecord
    lua.set_app_data(run_id.to_string());

    // Human-input transport for crew:ask_human() — carries the run_id so the
    // method can flip the run between Running and WaitingForInput, plus the
    // store + bus the agent-facing ask_human tool needs inside crew:run().
    if let Some(bridge) = input_bridge {
        lua.set_app_data(crate::engine::input_bridge::AskHumanContext {
            bridge,
            run_id: Some(run_id.to_string()),
            store: shared_store.clone(),
            eventbus: Some(eventbus.clone()),
        });
    }

    // Inject the server-wide store singleton so `LuaCrew` prefills its
    // OnceCell instead of bootstrapping a new Postgres pool per run.
    if let Some(store) = shared_store.clone() {
        lua.set_app_data(store);
    }

    // Inject input as a global `input` table (from the HTTP request body)
    if let Some(input_value) = input {
        // Extract tags from input if present (e.g., {"tags": ["v2", "experiment"], ...})
        let tag_strings = validate_run_tags(Some(input_value))?;
        if !tag_strings.is_empty() {
            lua.set_app_data(tag_strings);
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
    let script = crate::lua::source::read_lua_source(entrypoint)?;

    let exec_err = {
        let _execution =
            crate::lua::limits::LuaExecutionGuard::begin(&lua).map_err(IronCrewError::Lua)?;
        lua.load(&script).exec_async().await.err()
    };

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
    let script = crate::lua::source::read_lua_source(entrypoint)?;

    {
        let _execution =
            crate::lua::limits::LuaExecutionGuard::begin(&lua).map_err(IronCrewError::Lua)?;
        lua.load(&script)
            .exec_async()
            .await
            .map_err(IronCrewError::Lua)?;
    }

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
    Path((flow, run_id)): Path<(String, String)>,
    headers: HeaderMap,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    validate_run_id(&run_id)
        .map_err(|error| error_response(StatusCode::BAD_REQUEST, sanitize_error(&error)))?;
    // Scope to the flow in the URL: resolve it to the canonical slug and only
    // abort a run that belongs to it, so `DELETE /flows/A/runs/{id}` can't
    // cancel flow B's run.
    let result: Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> =
        match resolve_flow_path(&state, &flow) {
            Err(e) => Err(error_response(flow_status(&e), sanitize_error(&e))),
            Ok(p) => {
                let flow_slug = p
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("")
                    .to_string();
                let active_runs = state.active_runs.read().await;
                match active_runs.get(&run_id) {
                    Some(active_run) if active_run.flow == flow_slug => {
                        active_run.abort_handle.abort();
                        tracing::info!("Run {} aborted by client", run_id);
                        Ok(Json(serde_json::json!({
                            "run_id": run_id,
                            "status": "aborted",
                        })))
                    }
                    // Found but belongs to another flow → same 404 as
                    // truly-missing, so the endpoint doesn't confirm the run
                    // exists under a different flow.
                    _ => Err(error_response(
                        StatusCode::NOT_FOUND,
                        format!("Run '{}' not found or already completed", run_id),
                    )),
                }
            }
        };

    let (success, status_code) = match &result {
        Ok(_) => (true, 200u16),
        Err((sc, _)) => (false, sc.as_u16()),
    };

    crate::api::audit::record(
        &state.store,
        "flow.run.abort",
        Some(&flow),
        Some(&run_id),
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
// Human-in-the-loop: pending questions + answers (crew:ask_human)
// ---------------------------------------------------------------------------

/// `GET /flows/{flow}/questions/{run_id}` — pending `ask_human` questions for
/// a live run. Lets a UI that missed the SSE `human_input_requested` event
/// (or a poll-only client) recover state. Flow-scoped like `abort_run`.
pub async fn list_questions(
    State(state): State<Arc<AppState>>,
    Path((flow, run_id)): Path<(String, String)>,
    headers: HeaderMap,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    validate_run_id(&run_id)
        .map_err(|error| error_response(StatusCode::BAD_REQUEST, sanitize_error(&error)))?;
    let result: Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> =
        match resolve_flow_path(&state, &flow) {
            Err(e) => Err(error_response(flow_status(&e), sanitize_error(&e))),
            Ok(p) => {
                let flow_slug = p
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("")
                    .to_string();
                let active_runs = state.active_runs.read().await;
                match active_runs.get(&run_id) {
                    Some(active_run) if active_run.flow == flow_slug => {
                        let questions = active_run.input_bridge.list();
                        let status = if questions.is_empty() {
                            "running"
                        } else {
                            "waiting_for_input"
                        };
                        Ok(Json(serde_json::json!({
                            "run_id": run_id,
                            "status": status,
                            "questions": questions,
                        })))
                    }
                    // Found but belongs to another flow → same 404 as
                    // truly-missing (don't confirm existence across flows).
                    _ => Err(error_response(
                        StatusCode::NOT_FOUND,
                        format!("Run '{}' not found or already completed", run_id),
                    )),
                }
            }
        };

    let (success, status_code) = match &result {
        Ok(_) => (true, 200u16),
        Err((sc, _)) => (false, sc.as_u16()),
    };
    crate::api::audit::record(
        &state.store,
        "flow.run.questions_list",
        Some(&flow),
        Some(&run_id),
        &headers,
        Some(addr),
        success,
        status_code,
        None,
    )
    .await;

    result
}

#[derive(serde::Deserialize)]
pub struct AnswerRequest {
    pub question_id: String,
    pub answer: serde_json::Value,
}

/// `POST /flows/{flow}/answer/{run_id}` — deliver a human answer to a pending
/// `ask_human` question; the suspended flow coroutine resumes with the value.
/// First writer wins; a repeat answer gets 404 (the question is gone). The
/// audit record carries the question_id but never the answer body — answers
/// may contain secrets.
pub async fn answer_question(
    State(state): State<Arc<AppState>>,
    Path((flow, run_id)): Path<(String, String)>,
    headers: HeaderMap,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Json(body): Json<AnswerRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    validate_run_id(&run_id)
        .map_err(|error| error_response(StatusCode::BAD_REQUEST, sanitize_error(&error)))?;
    let question_id = body.question_id.clone();
    let result: Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> =
        match resolve_flow_path(&state, &flow) {
            Err(e) => Err(error_response(flow_status(&e), sanitize_error(&e))),
            Ok(p) => {
                let flow_slug = p
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("")
                    .to_string();
                let active_runs = state.active_runs.read().await;
                match active_runs.get(&run_id) {
                    Some(active_run) if active_run.flow == flow_slug => {
                        match active_run.input_bridge.answer(&question_id, body.answer) {
                            Ok(()) => Ok(Json(serde_json::json!({
                                "run_id": run_id,
                                "question_id": question_id,
                                "status": "delivered",
                            }))),
                            Err(_) => Err(error_response(
                                StatusCode::NOT_FOUND,
                                format!(
                                    "Question '{}' not found or expired on run '{}'",
                                    question_id, run_id
                                ),
                            )),
                        }
                    }
                    _ => Err(error_response(
                        StatusCode::NOT_FOUND,
                        format!("Run '{}' not found or already completed", run_id),
                    )),
                }
            }
        };

    let (success, status_code) = match &result {
        Ok(_) => (true, 200u16),
        Err((sc, _)) => (false, sc.as_u16()),
    };
    crate::api::audit::record(
        &state.store,
        "flow.run.question_answer",
        Some(&flow),
        Some(&run_id),
        &headers,
        Some(addr),
        success,
        status_code,
        Some(serde_json::json!({ "question_id": question_id })),
    )
    .await;

    result
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
        CrewEvent::HumanInputRequested { .. } => "human_input_requested",
        CrewEvent::HumanInputReceived { .. } => "human_input_received",
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
    validate_run_id(&run_id)
        .map_err(|error| error_response(StatusCode::BAD_REQUEST, sanitize_error(&error)))?;
    let flow_path = resolve_flow_path(&state, &flow)
        .map_err(|e| error_response(flow_status(&e), sanitize_error(&e)))?;
    let flow_slug = flow_path
        .file_name()
        .and_then(|segment| segment.to_str())
        .unwrap_or("");

    let sse_permit = state.sse_permits.clone().try_acquire_owned().map_err(|_| {
        error_response(
            StatusCode::TOO_MANY_REQUESTS,
            format!(
                "SSE connection limit reached ({})",
                state.max_sse_connections
            ),
        )
    })?;

    let active_runs = state.active_runs.read().await;
    let active_run = active_runs.get(&run_id).ok_or_else(|| {
        error_response(
            StatusCode::NOT_FOUND,
            format!("Run '{}' not found or already completed", run_id),
        )
    })?;
    if active_run.flow != flow_slug {
        return Err(error_response(
            StatusCode::NOT_FOUND,
            format!("Run '{}' not found or already completed", run_id),
        ));
    }

    // Subscribe and snapshot under one EventBus critical section so an event
    // cannot land in the replay/subscription gap.
    let (replay, mut rx) = active_run.eventbus.subscribe_with_replay();
    drop(active_runs);

    // Optional output truncation (disabled by default)
    let sse_max_chars: Option<usize> = std::env::var("IRONCREW_SSE_OUTPUT_MAX_CHARS")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|&n| n > 0);

    let stream = async_stream::stream! {
        let _sse_permit = sse_permit;
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

/// Cap on the number of simultaneously-active runs — reads
/// `IRONCREW_MAX_ACTIVE_RUNS` once at boot (default 4).
pub fn max_active_runs() -> usize {
    positive_bounded_env("IRONCREW_MAX_ACTIVE_RUNS", 4, 1024)
}

/// Global cap for long-lived run and conversation SSE connections.
pub fn max_sse_connections() -> usize {
    positive_bounded_env("IRONCREW_MAX_SSE_CONNECTIONS", 16, 1024)
}

/// Maximum wall-clock lifetime for an HTTP run.
pub fn max_run_lifetime() -> std::time::Duration {
    std::time::Duration::from_secs(positive_bounded_env(
        "IRONCREW_MAX_RUN_LIFETIME",
        30 * 60,
        24 * 60 * 60,
    ) as u64)
}

/// How long a completed run's event bus remains available for a late SSE
/// subscriber. Defaults to the established five-second recovery window.
fn run_sse_retention() -> std::time::Duration {
    std::time::Duration::from_secs(
        positive_bounded_env("IRONCREW_RUN_SSE_RETENTION_SECS", 5, 300) as u64,
    )
}

/// Default page size for `GET /flows/{flow}/runs` — override with `IRONCREW_RUNS_DEFAULT_LIMIT`.
fn runs_default_limit() -> usize {
    positive_bounded_env("IRONCREW_RUNS_DEFAULT_LIMIT", 20, runs_max_limit())
}

/// Hard cap on page size — override with `IRONCREW_RUNS_MAX_LIMIT`.
/// A client that asks for more than this gets silently clamped.
fn runs_max_limit() -> usize {
    positive_bounded_env("IRONCREW_RUNS_MAX_LIMIT", 100, 1000)
}

pub async fn list_runs(
    State(state): State<Arc<AppState>>,
    Path(flow): Path<String>,
    Query(params): Query<ListRunsQuery>,
) -> Result<Json<ListRunsResponse>, (StatusCode, Json<ErrorResponse>)> {
    // Resolve the flow to its canonical slug and scope the query to it, so
    // `GET /flows/A/runs` returns only flow A's runs (the store is a
    // server-wide singleton shared across flows).
    let flow_path = resolve_flow_path(&state, &flow)
        .map_err(|e| error_response(flow_status(&e), sanitize_error(&e)))?;
    let flow_slug = flow_path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_string();
    let store = &state.store;

    let default_limit = runs_default_limit();
    let max_limit = runs_max_limit();
    let limit = params.limit.unwrap_or(default_limit).min(max_limit).max(1);
    let offset = params.offset.unwrap_or(0);

    let filter = crate::engine::run_history::ListRunsFilter {
        flow: Some(flow_slug),
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
    validate_run_id(&id)
        .map_err(|error| error_response(StatusCode::BAD_REQUEST, sanitize_error(&error)))?;
    let flow_path = resolve_flow_path(&state, &flow)
        .map_err(|e| error_response(flow_status(&e), sanitize_error(&e)))?;
    let flow_slug = flow_path.file_name().and_then(|s| s.to_str()).unwrap_or("");

    let record = state
        .store
        .get_run(&id)
        .await
        .map_err(|e| error_response(StatusCode::NOT_FOUND, e.to_string()))?;

    // Scope by flow: a run launched under a different flow is invisible here,
    // reported as 404 rather than confirming it exists elsewhere.
    if record.flow != flow_slug {
        return Err(error_response(
            StatusCode::NOT_FOUND,
            format!("Run '{}' not found", id),
        ));
    }

    Ok(Json(record))
}

pub async fn delete_run(
    State(state): State<Arc<AppState>>,
    Path((flow, id)): Path<(String, String)>,
    headers: HeaderMap,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    validate_run_id(&id)
        .map_err(|error| error_response(StatusCode::BAD_REQUEST, sanitize_error(&error)))?;
    let result: Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> = async {
        let flow_path = resolve_flow_path(&state, &flow)
            .map_err(|e| error_response(flow_status(&e), sanitize_error(&e)))?;
        let flow_slug = flow_path.file_name().and_then(|s| s.to_str()).unwrap_or("");

        // Verify the run belongs to this flow before deleting, so
        // `DELETE /flows/A/runs/{id}` can't remove flow B's record. A run from
        // another flow reads as 404, same as a missing one.
        let record = state
            .store
            .get_run(&id)
            .await
            .map_err(|e| error_response(StatusCode::NOT_FOUND, e.to_string()))?;
        if record.flow != flow_slug {
            return Err(error_response(
                StatusCode::NOT_FOUND,
                format!("Run '{}' not found", id),
            ));
        }

        let active_in_memory = state
            .active_runs
            .read()
            .await
            .get(&id)
            .is_some_and(|active| !*active.terminal.borrow());
        if active_in_memory || record.status.is_in_flight() {
            return Err(error_response(
                StatusCode::CONFLICT,
                format!("Run '{}' is still active and cannot be deleted", id),
            ));
        }

        state
            .store
            .delete_run(&id)
            .await
            .map_err(|e| error_response(StatusCode::NOT_FOUND, e.to_string()))?;
        Ok(Json(serde_json::json!({"deleted": id})))
    }
    .await;

    let (success, status_code) = match &result {
        Ok(_) => (true, 200u16),
        Err((sc, _)) => (false, sc.as_u16()),
    };

    crate::api::audit::record(
        &state.store,
        "flow.run.delete",
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
        if let Ok(script) = crate::lua::source::read_lua_source(ep) {
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
    use super::{
        RunCrewResponse, TerminalPersistence, classify_work_result, persist_terminal_outcome,
        truncate_utf8, validate_run_id, validate_run_tags,
    };
    use crate::engine::run_history::{JsonFileStore, RunStatus};
    use crate::engine::store::StateStore;
    use crate::utils::error::IronCrewError;
    use std::sync::Arc;

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

    #[test]
    fn run_ids_accept_only_bounded_ascii_identifiers() {
        assert!(validate_run_id("run-1.test_value").is_ok());
        assert!(validate_run_id("").is_err());
        assert!(validate_run_id(&"a".repeat(129)).is_err());
        assert!(validate_run_id("run/escape").is_err());
        assert!(validate_run_id("rün").is_err());
    }

    #[test]
    fn run_tags_are_strict_and_hard_bounded() {
        let valid = serde_json::json!({"tags": ["release", "railway-pro"]});
        assert_eq!(
            validate_run_tags(Some(&valid)).unwrap(),
            vec!["release", "railway-pro"]
        );

        assert!(validate_run_tags(Some(&serde_json::json!({"tags": "release"}))).is_err());
        assert!(validate_run_tags(Some(&serde_json::json!({"tags": ["same", "same"]}))).is_err());
        assert!(validate_run_tags(Some(&serde_json::json!({"tags": [1]}))).is_err());

        let too_many = serde_json::json!({
            "tags": (0..=super::HARD_API_MAX_TAGS)
                .map(|index| format!("tag-{index}"))
                .collect::<Vec<_>>()
        });
        assert!(validate_run_tags(Some(&too_many)).is_err());
        let too_large = serde_json::json!({
            "tags": ["x".repeat(super::HARD_API_MAX_TAG_BYTES + 1)]
        });
        assert!(validate_run_tags(Some(&too_large)).is_err());
    }

    #[tokio::test]
    async fn panicked_work_is_persisted_as_failed() {
        let handle: tokio::task::JoinHandle<std::result::Result<RunCrewResponse, IronCrewError>> =
            tokio::spawn(async { panic!("intentional monitor test panic") });
        let outcome = classify_work_result(handle.await, 42);
        assert_eq!(outcome.status, RunStatus::Failed);
        assert!(
            outcome
                .error_message
                .as_deref()
                .is_some_and(|message| message.contains("Task panicked"))
        );

        let temp = tempfile::tempdir().unwrap();
        let store: Arc<dyn StateStore> =
            Arc::new(JsonFileStore::new(temp.path().join(".ironcrew")).unwrap());
        let status = persist_terminal_outcome(
            &store,
            TerminalPersistence {
                run_id: "panic-run",
                flow: "panic-flow",
                started_at: "2026-07-18T00:00:00Z",
                tags: &[],
                status: outcome.status,
                duration_ms: outcome.duration_ms,
                total_tokens: outcome.total_tokens,
            },
        )
        .await
        .unwrap();

        assert_eq!(status, RunStatus::Failed);
        assert_eq!(
            store.get_run("panic-run").await.unwrap().status,
            RunStatus::Failed
        );
    }
}

// ---------------------------------------------------------------------------
// Audit log read endpoint
// ---------------------------------------------------------------------------

#[derive(Debug, serde::Deserialize)]
pub struct ListAuditQuery {
    pub flow: Option<String>,
    pub action: Option<String>,
    pub actor: Option<String>,
    pub since: Option<String>,
    pub until: Option<String>,
    pub success: Option<bool>,
    pub limit: Option<usize>,
    pub offset: Option<usize>,
}

#[derive(Debug, serde::Serialize)]
pub struct ListAuditResponse {
    pub events: Vec<crate::engine::audit::AuditEvent>,
    pub total: u64,
    pub limit: usize,
    pub offset: usize,
}

pub async fn list_audit(
    State(state): State<Arc<AppState>>,
    axum::extract::Query(params): axum::extract::Query<ListAuditQuery>,
) -> Result<Json<ListAuditResponse>, (StatusCode, Json<ErrorResponse>)> {
    let default_limit: usize = std::env::var("IRONCREW_AUDIT_DEFAULT_LIMIT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(50);
    let max_limit: usize = std::env::var("IRONCREW_AUDIT_MAX_LIMIT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(500);
    let limit = params.limit.unwrap_or(default_limit).min(max_limit);
    let offset = params.offset.unwrap_or(0);

    let filter = crate::engine::audit::AuditFilter {
        flow_path: params.flow,
        action: params.action,
        actor: params.actor,
        since: params.since,
        until: params.until,
        success: params.success,
    };

    let events = state
        .store
        .list_audit_events(&filter, limit, offset)
        .await
        .map_err(|e| error_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let total = state
        .store
        .count_audit_events(&filter)
        .await
        .map_err(|e| error_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(ListAuditResponse {
        events,
        total,
        limit,
        offset,
    }))
}
