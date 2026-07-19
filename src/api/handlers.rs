use axum::{
    extract::{ConnectInfo, Extension, Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::sse::{Event, KeepAlive, Sse},
    response::{IntoResponse, Json, Response},
};
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast;

use crate::engine::eventbus::{CrewEvent, DurableEventPersistence, EventBus};
use crate::engine::human_input::{HumanInputAnswerOutcome, HumanInputListOutcome};
use crate::engine::idempotency::{
    IdempotencyClaim, IdempotencyClaimOutcome, IdempotencyCompletion, IdempotencyLookup,
    IdempotencyQuotaResource, IdempotencyQuotaScope, IdempotencyRecord, PrincipalId, RUN_OPERATION,
    RunCancellationRequest, RunFenceHeartbeat, RunIntentSignal,
};
use crate::engine::input_bridge::{AnswerError, validate_http_answer_size};
use crate::engine::run_events::{
    EventJournalScope, RunEventCursor, RunEventCursorError, RunEventPage,
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
    let mut body: serde_json::Value = serde_json::from_str(body).map_err(|error| {
        tracing::error!(%error, "Stored run idempotency response is corrupt");
        error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Stored idempotency response is corrupt".into(),
        )
    })?;
    let object = body.as_object_mut().ok_or_else(|| {
        tracing::error!("Stored run idempotency response is not a JSON object");
        error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Stored idempotency response is corrupt".into(),
        )
    })?;
    // Older retained responses predate replica diagnostics. Enrich them at
    // replay time without changing the durable idempotency payload so every
    // caller can identify the process that accepted the run.
    object.insert(
        "owner_instance_id".into(),
        serde_json::json!(record.owner_instance_id),
    );
    object.insert("control_scope".into(), serde_json::json!("process"));
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

/// Authenticated runtime-capability snapshot. This deliberately separates
/// shared durable records from the live control objects that still reside in
/// one process, so operators and clients cannot mistake a shared store for a
/// distributed execution plane.
pub async fn capabilities(
    State(state): State<Arc<AppState>>,
) -> (HeaderMap, Json<serde_json::Value>) {
    let mut headers = HeaderMap::new();
    headers.insert(
        axum::http::header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static("no-store"),
    );
    let human_input_scope = if state.store.supports_durable_human_input() {
        "shared_store_for_keyed_runs"
    } else {
        "process"
    };
    let sse_replay_scope = match state.store.event_journal_scope() {
        EventJournalScope::SharedStore => "shared_store",
        EventJournalScope::ProcessLocal => "process",
    };
    (
        headers,
        Json(serde_json::json!({
            "version": env!("CARGO_PKG_VERSION"),
            "instance_id": state.store.instance_id(),
            "topology": "single_executor",
            "control_scope": "process",
            "multi_replica_control": false,
            "live_control": {
                "run_abort": {
                    "local": "process",
                    "cross_instance": "keyed_store_if_supported",
                },
                "human_input": human_input_scope,
                "sse_replay": sse_replay_scope,
                "conversations": "process",
            },
        })),
    )
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

#[derive(Debug)]
enum DurableRunLocation {
    Missing,
    Terminal(Box<crate::engine::run_history::RunRecord>),
    ActiveOnThisInstance,
    ActiveOwnerUnknown,
    ActiveOnOtherInstance(String),
}

/// Classify a miss in the process-local active-run registry using the durable
/// record. Flow scoping is checked before ownership is exposed so a caller can
/// never use control endpoints to discover another flow's run or owner.
async fn durable_run_location(
    state: &AppState,
    flow_slug: &str,
    run_id: &str,
) -> Result<DurableRunLocation, IronCrewError> {
    let record = match state.store.get_run(run_id).await {
        Ok(record) => record,
        Err(error) if run_not_found(&error, run_id) => return Ok(DurableRunLocation::Missing),
        Err(error) => return Err(error),
    };
    if record.flow != flow_slug {
        return Ok(DurableRunLocation::Missing);
    }
    if record.status.is_terminal() {
        return Ok(DurableRunLocation::Terminal(Box::new(record)));
    }
    if record.owner_instance_id.is_empty() {
        Ok(DurableRunLocation::ActiveOwnerUnknown)
    } else if record.owner_instance_id == state.store.instance_id() {
        Ok(DurableRunLocation::ActiveOnThisInstance)
    } else {
        Ok(DurableRunLocation::ActiveOnOtherInstance(
            record.owner_instance_id,
        ))
    }
}

fn structured_error(
    status: StatusCode,
    message: impl Into<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    (status, Json(serde_json::json!({ "error": message.into() })))
}

fn foreign_run_owner_error(
    run_id: &str,
    owner_instance_id: String,
) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::CONFLICT,
        Json(serde_json::json!({
            "error": "Run is active on another IronCrew instance",
            "code": "run_owned_by_another_instance",
            "run_id": run_id,
            "owner_instance_id": owner_instance_id,
            "control_scope": "process",
            "retryable": true,
        })),
    )
}

fn local_run_control_unavailable_error(
    run_id: &str,
    owner_instance_id: &str,
) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(serde_json::json!({
            "error": "Run control is temporarily unavailable on its owner instance",
            "code": "run_control_temporarily_unavailable",
            "run_id": run_id,
            "owner_instance_id": owner_instance_id,
            "control_scope": "process",
            "retryable": true,
        })),
    )
}

fn run_location_error(
    state: &AppState,
    run_id: &str,
    location: DurableRunLocation,
) -> (StatusCode, Json<serde_json::Value>) {
    match location {
        DurableRunLocation::ActiveOnOtherInstance(owner) => foreign_run_owner_error(run_id, owner),
        DurableRunLocation::ActiveOnThisInstance => {
            local_run_control_unavailable_error(run_id, state.store.instance_id())
        }
        DurableRunLocation::ActiveOwnerUnknown => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({
                "error": "Run control owner is unavailable",
                "code": "run_control_owner_unknown",
                "run_id": run_id,
                "control_scope": "process",
                "retryable": true,
            })),
        ),
        DurableRunLocation::Missing | DurableRunLocation::Terminal(_) => structured_error(
            StatusCode::NOT_FOUND,
            format!("Run '{}' not found or already completed", run_id),
        ),
    }
}

fn run_location_store_error(error: IronCrewError) -> (StatusCode, Json<serde_json::Value>) {
    tracing::warn!(%error, "Failed to classify durable run ownership");
    structured_error(
        StatusCode::SERVICE_UNAVAILABLE,
        "Run ownership is temporarily unavailable",
    )
}

async fn request_foreign_run_abort(
    state: &AppState,
    flow_slug: &str,
    run_id: &str,
    observed_owner: String,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    match state
        .store
        .request_run_cancellation(run_id, flow_slug)
        .await
        .map_err(run_location_store_error)?
    {
        RunCancellationRequest::Requested {
            owner_instance_id,
            already_requested,
        } => Ok(Json(serde_json::json!({
            "run_id": run_id,
            "status": "cancellation_requested",
            "owner_instance_id": owner_instance_id,
            "control_scope": "shared_store",
            "already_requested": already_requested,
        }))),
        RunCancellationRequest::Terminal(status) => Ok(Json(serde_json::json!({
            "run_id": run_id,
            "status": status.to_string(),
            "terminal": true,
        }))),
        RunCancellationRequest::NotFound => Err(structured_error(
            StatusCode::NOT_FOUND,
            format!("Run '{}' not found or already completed", run_id),
        )),
        RunCancellationRequest::NotDurable => Err(foreign_run_owner_error(run_id, observed_owner)),
    }
}

// A terminal completion may contain up to the aggregate run-results ceiling.
// Retaining that payload through an unbounded storage outage would multiply
// it by every admitted run. Keep at most 1 MiB per run for one extra full
// retry; larger payloads get their normal first write attempt only.
const TERMINAL_RESULT_RETRY_RETAINED_BYTES: usize = 1024 * 1024;
const TERMINAL_RESULT_FULL_FAILURE_LIMIT: usize = 2;

fn retained_task_result_bytes(
    results: &[crate::engine::task::TaskResult],
    result_capacity: usize,
) -> usize {
    result_capacity
        .saturating_mul(std::mem::size_of::<crate::engine::task::TaskResult>())
        .saturating_add(results.iter().fold(0usize, |total, result| {
            total
                .saturating_add(result.task.capacity())
                .saturating_add(result.agent.capacity())
                .saturating_add(result.output.capacity())
                .saturating_add(result.reasoning.as_ref().map(String::capacity).unwrap_or(0))
        }))
}

struct ReleasedTerminalResults {
    result_count: usize,
    retained_bytes: usize,
    full_failures: usize,
}

struct TerminalResultRetention {
    completion: Option<RunCompletion>,
    full_failures: usize,
}

impl TerminalResultRetention {
    fn new(completion: Option<RunCompletion>) -> Self {
        Self {
            completion,
            full_failures: 0,
        }
    }

    fn completion(&self) -> Option<&RunCompletion> {
        self.completion.as_ref()
    }

    /// Record a failed persistence attempt and release task payloads once the
    /// bounded retry allowance is exhausted. Status, timing, and aggregate
    /// token counts stay resident and continue retrying until durable.
    fn record_failure(&mut self) -> Option<ReleasedTerminalResults> {
        let completion = self.completion.as_mut()?;
        if completion.task_results.is_empty() {
            return None;
        }

        self.full_failures = self.full_failures.saturating_add(1);
        let retained_bytes = retained_task_result_bytes(
            &completion.task_results,
            completion.task_results.capacity(),
        );
        if retained_bytes <= TERMINAL_RESULT_RETRY_RETAINED_BYTES
            && self.full_failures < TERMINAL_RESULT_FULL_FAILURE_LIMIT
        {
            return None;
        }

        let result_count = completion.task_results.len();
        completion.task_results = Vec::new();
        Some(ReleasedTerminalResults {
            result_count,
            retained_bytes,
            full_failures: self.full_failures,
        })
    }
}

/// Persist one terminal transition for an HTTP-owned run. A normal
/// `crew:run()` contributes its staged, result-bearing completion only after
/// the enclosing Lua entrypoint ends. If a task failed before Lua could create
/// the intent, create a minimal fallback record only after confirming the run
/// is genuinely absent.
struct TerminalPersistence<'a> {
    run_id: &'a str,
    flow: &'a str,
    started_at: &'a str,
    tags: &'a [String],
    status: RunStatus,
    duration_ms: u64,
    total_tokens: u32,
    /// Full crew completion retained by the API lifecycle until the enclosing
    /// Lua entrypoint has returned. Absent for pre-crew failures, aborts,
    /// timeouts, and flows that never call `crew:run()`.
    completion: Option<&'a RunCompletion>,
}

async fn persist_terminal_outcome(
    store: &Arc<dyn crate::engine::store::StateStore>,
    terminal: TerminalPersistence<'_>,
) -> Result<RunStatus, IronCrewError> {
    let synthesized;
    let completion = match terminal.completion {
        Some(completion) => completion,
        None => {
            synthesized = RunCompletion {
                status: terminal.status.clone(),
                finished_at: chrono::Utc::now().to_rfc3339(),
                duration_ms: terminal.duration_ms,
                task_results: Vec::new(),
                total_tokens: terminal.total_tokens,
                cached_tokens: 0,
            };
            &synthesized
        }
    };
    let completion_status = completion.status.clone();

    match store
        .update_run_completion(terminal.run_id, completion.clone())
        .await
    {
        Ok(RunTransition::Applied) => return Ok(completion_status.clone()),
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
        .update_run_completion(terminal.run_id, completion.clone())
        .await
    {
        Ok(RunTransition::Applied) => Ok(completion_status),
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

struct RunWorkResult {
    status: String,
    duration_ms: u64,
    total_tokens: u32,
}

struct RunExecutionContext {
    shared_store: Option<Arc<dyn crate::engine::store::StateStore>>,
    input_bridge: Option<Arc<crate::engine::input_bridge::InputBridge>>,
    run_intent_signal: Option<RunIntentSignal>,
    api_lifecycle: crate::lua::crew_userdata::ApiRunLifecycle,
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
        std::result::Result<RunWorkResult, IronCrewError>,
        tokio::task::JoinError,
    >,
    elapsed_ms: u64,
) -> WorkOutcome {
    match join_result {
        Ok(Ok(work)) => {
            let RunWorkResult {
                status: response_status,
                duration_ms: response_duration_ms,
                total_tokens: response_total_tokens,
            } = work;
            let status = response_status
                .parse::<RunStatus>()
                .ok()
                .filter(RunStatus::is_terminal)
                .unwrap_or(RunStatus::Success);
            WorkOutcome {
                status,
                duration_ms: response_duration_ms,
                total_tokens: response_total_tokens,
                error_message: None,
            }
        }
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
        "owner_instance_id": state.store.instance_id(),
        "control_scope": "process",
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
    let eventbus =
        EventBus::new_durable(256, state.store.clone(), flow_slug.clone(), run_id.clone());
    let started_at = chrono::Utc::now().to_rfc3339();
    let started = std::time::Instant::now();

    // Per-run human-input transport: crew:ask_human() parks on this, the
    // questions/answer endpoints reach it through ActiveRun.
    let input_bridge = Arc::new(if let Some(attempt) = idempotency_attempt.as_ref() {
        crate::engine::input_bridge::InputBridge::new_durable_http(
            state.store.clone(),
            flow_slug.clone(),
            run_id.clone(),
            attempt.key_hash.clone(),
            attempt.attempt_id.clone(),
        )
    } else {
        crate::engine::input_bridge::InputBridge::new(crate::engine::input_bridge::BridgeMode::Http)
    });
    // Retained by both the worker and its monitor. If outer Lua is aborted or
    // times out after `crew:run()` completed, the monitor can still preserve
    // those task results while applying the authoritative terminal status.
    let api_lifecycle = crate::lua::crew_userdata::ApiRunLifecycle::default();

    // Prepare the work task, then register it while holding the active-map
    // write lock. Rechecking readiness under that lock closes the race where
    // shutdown drains the map while a request is still being initialized.
    let eventbus_inner = eventbus.clone();
    let run_id_for_work = run_id.clone();
    let store_for_work = state.store.clone();
    let bridge_for_work = input_bridge.clone();
    let bridge_for_monitor = input_bridge.clone();
    let lifecycle_for_work = api_lifecycle.clone();
    let lifecycle_for_monitor = api_lifecycle;
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
                RunExecutionContext {
                    shared_store: Some(store_for_work),
                    input_bridge: Some(bridge_for_work),
                    run_intent_signal,
                    api_lifecycle: lifecycle_for_work,
                },
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

    // Monitor the work handle. It is the single API-level finalizer for normal
    // completion, errors, cancellation, panic, timeout, and server shutdown.
    // Store transitions remain compare-and-set so an external terminal writer
    // (for example an abort request) stays authoritative.
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
                    RunFenceHeartbeat::CancelRequested => {
                        tracing::info!(run_id = %run_id_clone, "Run worker stopped after a durable cancellation request");
                        (
                            RunStatus::Aborted,
                            started.elapsed().as_millis() as u64,
                            0,
                            None,
                            Some(RunFenceHeartbeat::CancelRequested),
                        )
                    }
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
        let staged_completion = lifecycle_for_monitor.take_completion().await;
        let run_completion = if matches!(&fence_result, Some(RunFenceHeartbeat::Terminal(_))) {
            // Another durable writer won. Its terminal payload is the fence;
            // never replace it with process-local staged task results.
            None
        } else {
            staged_completion.map(|mut staged| {
                staged.completion.status = requested_status.clone();
                staged.completion.finished_at = chrono::Utc::now().to_rfc3339();
                staged.completion.duration_ms = started.elapsed().as_millis() as u64;
                staged.completion
            })
        };
        let duration_ms = run_completion
            .as_ref()
            .map(|completion| completion.duration_ms)
            .unwrap_or(duration_ms);
        let total_tokens = run_completion
            .as_ref()
            .map(|completion| completion.total_tokens)
            .unwrap_or(total_tokens);
        let mut terminal_results = TerminalResultRetention::new(run_completion);
        let expired_questions = bridge_for_monitor.expire_all();
        if expired_questions > 0 {
            tracing::debug!(
                run_id = %run_id_clone,
                expired_questions,
                "Expired pending human-input questions after run termination"
            );
        }
        if let Some(message) = error_message {
            eventbus.emit(CrewEvent::Log {
                level: "error".into(),
                message,
            });
        }

        // Preserve journal ordering at the terminal fence. PostgreSQL rejects
        // ordinary event appends after the durable run record becomes
        // terminal, so drain every event emitted by the worker before writing
        // that record. This remains bounded: a degraded journal must never
        // prevent authoritative run finalization indefinitely.
        let event_flush = eventbus.flush_durable().await;
        if matches!(
            event_flush,
            DurableEventPersistence::Dropped
                | DurableEventPersistence::Failed
                | DurableEventPersistence::TimedOut
        ) {
            tracing::warn!(
                run_id = %run_id_clone,
                ?event_flush,
                "Run events were not fully durable before terminal persistence"
            );
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
                    completion: terminal_results.completion(),
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
                    if let Some(released) = terminal_results.record_failure() {
                        tracing::warn!(
                            run_id = %run_id_clone,
                            result_count = released.result_count,
                            retained_bytes = released.retained_bytes,
                            full_persistence_failures = released.full_failures,
                            "Released staged task results after terminal persistence failures; terminal metadata will keep retrying without task outputs"
                        );
                    }
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

        let event_persistence = eventbus
            .emit_terminal(CrewEvent::RunComplete {
                run_id: run_id_clone.clone(),
                status: terminal_status.to_string(),
                duration_ms,
                total_tokens,
            })
            .await;
        if matches!(
            event_persistence,
            DurableEventPersistence::Dropped
                | DurableEventPersistence::Failed
                | DurableEventPersistence::TimedOut
        ) {
            tracing::warn!(
                run_id = %run_id_clone,
                ?event_persistence,
                "Terminal run record is durable but its replay-journal event was not confirmed"
            );
        }
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
    context: RunExecutionContext,
) -> std::result::Result<RunWorkResult, IronCrewError> {
    use crate::cli::project::{load_project, setup_crew_runtime};
    use crate::lua::api::json_value_to_lua;

    let RunExecutionContext {
        shared_store,
        input_bridge,
        run_intent_signal,
        api_lifecycle,
    } = context;

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

    // Unlike a CLI invocation, an HTTP run owns the complete Lua entrypoint.
    // `crew:run()` stages its rich completion here so flow-level Lua can keep
    // running (and can suspend on post-crew human input) while the durable run
    // remains in-flight. The API monitor performs the terminal write after
    // this worker returns.
    lua.set_app_data(api_lifecycle.clone());

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
    // the crew may have completed successfully. Prefer its staged completion,
    // preserving the historical behavior where that crew outcome wins.
    let staged_completion = api_lifecycle.completion_summary().await;
    let run_id: Option<String> = lua.globals().get("__ironcrew_last_run_id").ok();

    // Read the recorded run directly so concurrent executions cannot swap results.
    if let Some(run_id) = run_id {
        let store = match shared_store.clone() {
            Some(s) => s,
            None => create_store(loader.project_dir().join(".ironcrew")).await?,
        };
        let run = store.get_run(&run_id).await?;
        if staged_completion
            .as_ref()
            .is_some_and(|staged| staged.run_id != run_id)
        {
            return Err(IronCrewError::Validation(
                "HTTP run staged completion for a different run id".into(),
            ));
        }
        let status = staged_completion
            .as_ref()
            .map(|staged| staged.status.to_string())
            .unwrap_or_else(|| run.status.to_string());
        let duration_ms = staged_completion
            .as_ref()
            .map(|staged| staged.duration_ms)
            .unwrap_or(run.duration_ms);
        let total_tokens = staged_completion
            .as_ref()
            .map(|staged| staged.total_tokens)
            .unwrap_or(run.total_tokens);
        return Ok(RunWorkResult {
            status,
            duration_ms,
            total_tokens,
        });
    }

    // No run record found — if the Lua script failed, propagate the error
    if let Some(err) = exec_err {
        return Err(IronCrewError::Lua(err));
    }

    Ok(RunWorkResult {
        status: "completed".into(),
        duration_ms: 0,
        total_tokens: 0,
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
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    validate_run_id(&run_id)
        .map_err(|error| structured_error(StatusCode::BAD_REQUEST, sanitize_error(&error)))?;
    // Scope to the flow in the URL: resolve it to the canonical slug and only
    // abort a run that belongs to it, so `DELETE /flows/A/runs/{id}` can't
    // cancel flow B's run.
    let result: Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> = async {
        let path = resolve_flow_path(&state, &flow)
            .map_err(|error| structured_error(flow_status(&error), sanitize_error(&error)))?;
        let flow_slug = path
            .file_name()
            .and_then(|segment| segment.to_str())
            .unwrap_or("")
            .to_string();

        let handled_locally = {
            let active_runs = state.active_runs.read().await;
            match active_runs.get(&run_id) {
                Some(active_run) if active_run.flow == flow_slug => {
                    active_run.abort_handle.abort();
                    active_run.input_bridge.expire_all();
                    true
                }
                Some(_) => {
                    return Err(structured_error(
                        StatusCode::NOT_FOUND,
                        format!("Run '{}' not found or already completed", run_id),
                    ));
                }
                None => false,
            }
        };
        if handled_locally {
            tracing::info!("Run {} aborted by client", run_id);
            return Ok(Json(serde_json::json!({
                "run_id": run_id,
                "status": "aborted",
            })));
        };

        let location = durable_run_location(&state, &flow_slug, &run_id)
            .await
            .map_err(run_location_store_error)?;
        match location {
            DurableRunLocation::ActiveOnOtherInstance(owner) => {
                request_foreign_run_abort(&state, &flow_slug, &run_id, owner).await
            }
            location => Err(run_location_error(&state, &run_id, location)),
        }
    }
    .await;

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

type HumanInputHttpResponse = (StatusCode, HeaderMap, Json<serde_json::Value>);

fn human_input_response(status: StatusCode, value: serde_json::Value) -> HumanInputHttpResponse {
    let mut headers = HeaderMap::new();
    headers.insert(
        axum::http::header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static("no-store"),
    );
    (status, headers, Json(value))
}

/// `GET /flows/{flow}/questions/{run_id}` — pending `ask_human` questions for
/// a live run. Lets a UI that missed the SSE `human_input_requested` event
/// (or a poll-only client) recover state. Flow-scoped like `abort_run`.
pub async fn list_questions(
    State(state): State<Arc<AppState>>,
    Path((flow, run_id)): Path<(String, String)>,
    headers: HeaderMap,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
) -> Result<HumanInputHttpResponse, (StatusCode, Json<serde_json::Value>)> {
    validate_run_id(&run_id)
        .map_err(|error| structured_error(StatusCode::BAD_REQUEST, sanitize_error(&error)))?;
    let result: Result<HumanInputHttpResponse, (StatusCode, Json<serde_json::Value>)> = async {
        let path = resolve_flow_path(&state, &flow)
            .map_err(|error| structured_error(flow_status(&error), sanitize_error(&error)))?;
        let flow_slug = path
            .file_name()
            .and_then(|segment| segment.to_str())
            .unwrap_or("")
            .to_string();

        let local_bridge = {
            let active_runs = state.active_runs.read().await;
            match active_runs.get(&run_id) {
                Some(active_run) if active_run.flow == flow_slug => {
                    Some(active_run.input_bridge.clone())
                }
                Some(_) => {
                    return Err(structured_error(
                        StatusCode::NOT_FOUND,
                        format!("Run '{}' not found or already completed", run_id),
                    ));
                }
                None => None,
            }
        };
        if let Some(bridge) = local_bridge {
            if bridge.is_expired() {
                return Err(structured_error(
                    StatusCode::NOT_FOUND,
                    format!("Run '{}' not found or already completed", run_id),
                ));
            }
            let questions = bridge.list();
            let status = if questions.is_empty() {
                "running"
            } else {
                "waiting_for_input"
            };
            let control_scope = if bridge.supports_shared_human_input() {
                "shared_store"
            } else {
                "process"
            };
            return Ok(human_input_response(
                StatusCode::OK,
                serde_json::json!({
                    "run_id": run_id,
                    "status": status,
                    "owner_instance_id": state.store.instance_id(),
                    "control_scope": control_scope,
                    "questions": questions,
                }),
            ));
        };

        match state
            .store
            .list_human_inputs(&flow_slug, &run_id)
            .await
            .map_err(run_location_store_error)?
        {
            HumanInputListOutcome::Shared {
                owner_instance_id,
                questions,
            } => {
                let questions = questions
                    .into_iter()
                    .map(|question| question.info)
                    .collect::<Vec<_>>();
                let status = if questions.is_empty() {
                    "running"
                } else {
                    "waiting_for_input"
                };
                Ok(human_input_response(
                    StatusCode::OK,
                    serde_json::json!({
                        "run_id": run_id,
                        "status": status,
                        "owner_instance_id": owner_instance_id,
                        "control_scope": "shared_store",
                        "questions": questions,
                    }),
                ))
            }
            HumanInputListOutcome::NotDurable => {
                let location = durable_run_location(&state, &flow_slug, &run_id)
                    .await
                    .map_err(run_location_store_error)?;
                Err(run_location_error(&state, &run_id, location))
            }
        }
    }
    .await;

    let (success, status_code) = match &result {
        Ok((status, _, _)) => (true, status.as_u16()),
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
) -> Result<HumanInputHttpResponse, (StatusCode, Json<serde_json::Value>)> {
    validate_run_id(&run_id)
        .map_err(|error| structured_error(StatusCode::BAD_REQUEST, sanitize_error(&error)))?;
    let question_id = body.question_id.clone();
    let result: Result<HumanInputHttpResponse, (StatusCode, Json<serde_json::Value>)> = async {
        let path = resolve_flow_path(&state, &flow)
            .map_err(|error| structured_error(flow_status(&error), sanitize_error(&error)))?;
        let flow_slug = path
            .file_name()
            .and_then(|segment| segment.to_str())
            .unwrap_or("")
            .to_string();

        if question_id.is_empty()
            || question_id.len() > 128
            || question_id.chars().any(char::is_control)
        {
            return Err(structured_error(
                StatusCode::BAD_REQUEST,
                "Question id must be 1-128 printable characters",
            ));
        }

        match validate_http_answer_size(&body.answer) {
            Ok(()) => {}
            Err(AnswerError::TooLarge { max_bytes }) => {
                return Err(structured_error(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    format!("Question answer exceeds the {max_bytes}-byte limit"),
                ));
            }
            Err(AnswerError::Invalid(message)) => {
                return Err(structured_error(StatusCode::BAD_REQUEST, message));
            }
            Err(AnswerError::Unavailable(_) | AnswerError::UnknownOrExpired { .. }) => {
                unreachable!("answer preflight only validates serialized size")
            }
        }

        let local_bridge = {
            let active_runs = state.active_runs.read().await;
            match active_runs.get(&run_id) {
                Some(active_run) if active_run.flow == flow_slug => {
                    Some(active_run.input_bridge.clone())
                }
                Some(_) => {
                    return Err(structured_error(
                        StatusCode::NOT_FOUND,
                        format!("Run '{}' not found or already completed", run_id),
                    ));
                }
                None => None,
            }
        };
        if let Some(bridge) = local_bridge {
            return match bridge.answer_http(&question_id, body.answer).await {
                Ok(HumanInputAnswerOutcome::Queued { owner_instance_id }) => {
                    Ok(human_input_response(
                        StatusCode::ACCEPTED,
                        serde_json::json!({
                            "run_id": run_id,
                            "question_id": question_id,
                            "status": "queued",
                            "owner_instance_id": owner_instance_id,
                            "control_scope": "shared_store",
                        }),
                    ))
                }
                Ok(HumanInputAnswerOutcome::NotDurable) => Ok(human_input_response(
                    StatusCode::OK,
                    serde_json::json!({
                        "run_id": run_id,
                        "question_id": question_id,
                        "status": "delivered",
                        "owner_instance_id": state.store.instance_id(),
                        "control_scope": "process",
                    }),
                )),
                Ok(HumanInputAnswerOutcome::AlreadyAnswered) => Err(structured_error(
                    StatusCode::NOT_FOUND,
                    format!(
                        "Question '{}' not found or expired on run '{}'",
                        question_id, run_id
                    ),
                )),
                Ok(HumanInputAnswerOutcome::NotFound) => Err(structured_error(
                    StatusCode::NOT_FOUND,
                    format!(
                        "Question '{}' not found or expired on run '{}'",
                        question_id, run_id
                    ),
                )),
                Err(AnswerError::TooLarge { max_bytes }) => Err(structured_error(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    format!("Question answer exceeds the {max_bytes}-byte limit"),
                )),
                Err(AnswerError::Invalid(message)) => {
                    Err(structured_error(StatusCode::BAD_REQUEST, message))
                }
                Err(AnswerError::Unavailable(message)) => {
                    tracing::warn!(%message, "Durable human-input answer transport unavailable");
                    Err(structured_error(
                        StatusCode::SERVICE_UNAVAILABLE,
                        "Durable human-input transport is temporarily unavailable",
                    ))
                }
                Err(AnswerError::UnknownOrExpired { .. }) => Err(structured_error(
                    StatusCode::NOT_FOUND,
                    format!(
                        "Question '{}' not found or expired on run '{}'",
                        question_id, run_id
                    ),
                )),
            };
        };

        match state
            .store
            .answer_human_input(&flow_slug, &run_id, &question_id, &body.answer)
            .await
        {
            Ok(HumanInputAnswerOutcome::Queued { owner_instance_id }) => Ok(human_input_response(
                StatusCode::ACCEPTED,
                serde_json::json!({
                    "run_id": run_id,
                    "question_id": question_id,
                    "status": "queued",
                    "owner_instance_id": owner_instance_id,
                    "control_scope": "shared_store",
                }),
            )),
            Ok(HumanInputAnswerOutcome::AlreadyAnswered) => Err(structured_error(
                StatusCode::NOT_FOUND,
                format!(
                    "Question '{}' not found or expired on run '{}'",
                    question_id, run_id
                ),
            )),
            Ok(HumanInputAnswerOutcome::NotFound) => Err(structured_error(
                StatusCode::NOT_FOUND,
                format!(
                    "Question '{}' not found or expired on run '{}'",
                    question_id, run_id
                ),
            )),
            Ok(HumanInputAnswerOutcome::NotDurable) => {
                let location = durable_run_location(&state, &flow_slug, &run_id)
                    .await
                    .map_err(run_location_store_error)?;
                Err(run_location_error(&state, &run_id, location))
            }
            Err(error) => {
                tracing::warn!(%error, "Durable human-input answer enqueue failed");
                Err(structured_error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Durable human-input transport is temporarily unavailable",
                ))
            }
        }
    }
    .await;

    let (success, status_code) = match &result {
        Ok((status, _, _)) => (true, status.as_u16()),
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

fn cursor_error_response(error: RunEventCursorError) -> (StatusCode, Json<serde_json::Value>) {
    let (status, code) = match &error {
        RunEventCursorError::Ahead { .. } => (StatusCode::CONFLICT, "cursor_ahead"),
        RunEventCursorError::Expired { .. } => (StatusCode::CONFLICT, "cursor_expired"),
        RunEventCursorError::CrossRun => (StatusCode::BAD_REQUEST, "cursor_cross_run"),
        _ => (StatusCode::BAD_REQUEST, "invalid_cursor"),
    };
    (
        status,
        Json(serde_json::json!({
            "error": error.to_string(),
            "code": code,
        })),
    )
}

fn hardened_sse_response(mut response: Response) -> Response {
    response.headers_mut().insert(
        axum::http::header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static("no-store, no-transform"),
    );
    response.headers_mut().insert(
        axum::http::HeaderName::from_static("x-accel-buffering"),
        axum::http::HeaderValue::from_static("no"),
    );
    response
}

enum JournalPageReadError {
    TimedOut,
    Store(IronCrewError),
}

async fn read_journal_page(
    state: &AppState,
    flow: &str,
    run_id: &str,
    after_sequence: u64,
) -> std::result::Result<RunEventPage, JournalPageReadError> {
    match tokio::time::timeout(
        state.store.event_journal_config().read_timeout,
        state.store.read_run_events(flow, run_id, after_sequence),
    )
    .await
    {
        Ok(Ok(page)) => Ok(page),
        Ok(Err(error)) => Err(JournalPageReadError::Store(error)),
        Err(_) => Err(JournalPageReadError::TimedOut),
    }
}

fn journal_read_http_error(
    run_id: &str,
    error: JournalPageReadError,
) -> (StatusCode, Json<serde_json::Value>) {
    match error {
        JournalPageReadError::TimedOut => {
            tracing::warn!(run_id, "Initial durable run-event read timed out");
        }
        JournalPageReadError::Store(error) => {
            tracing::warn!(run_id, %error, "Initial durable run-event read failed");
        }
    }
    structured_error(
        StatusCode::SERVICE_UNAVAILABLE,
        "Run-event replay is temporarily unavailable",
    )
}

fn cursor_acknowledges_terminal(acknowledged_through: Option<u64>, sequence: u64) -> bool {
    acknowledged_through.is_some_and(|cursor_sequence| cursor_sequence >= sequence)
}

pub async fn flow_events(
    State(state): State<Arc<AppState>>,
    Path((flow, run_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Response, (StatusCode, Json<serde_json::Value>)> {
    validate_run_id(&run_id)
        .map_err(|error| structured_error(StatusCode::BAD_REQUEST, sanitize_error(&error)))?;
    let flow_path = resolve_flow_path(&state, &flow)
        .map_err(|error| structured_error(flow_status(&error), sanitize_error(&error)))?;
    let flow_slug = flow_path
        .file_name()
        .and_then(|segment| segment.to_str())
        .unwrap_or("")
        .to_string();

    let raw_cursor = headers
        .get(axum::http::HeaderName::from_static("last-event-id"))
        .map(|value| {
            value
                .to_str()
                .map(str::to_owned)
                .map_err(|_| cursor_error_response(RunEventCursorError::NonAscii))
        })
        .transpose()?;
    let cursor = raw_cursor
        .as_deref()
        .map(|value| RunEventCursor::parse_for_run(value, &run_id))
        .transpose()
        .map_err(cursor_error_response)?;

    let sse_permit = state.sse_permits.clone().try_acquire_owned().map_err(|_| {
        structured_error(
            StatusCode::TOO_MANY_REQUESTS,
            format!(
                "SSE connection limit reached ({})",
                state.max_sse_connections
            ),
        )
    })?;

    if state.store.event_journal_scope() == EventJournalScope::SharedStore {
        match durable_run_location(&state, &flow_slug, &run_id)
            .await
            .map_err(run_location_store_error)?
        {
            DurableRunLocation::Missing => {
                return Err(structured_error(
                    StatusCode::NOT_FOUND,
                    format!("Run '{}' not found or already completed", run_id),
                ));
            }
            DurableRunLocation::ActiveOnThisInstance
            | DurableRunLocation::ActiveOnOtherInstance(_)
            | DurableRunLocation::ActiveOwnerUnknown
            | DurableRunLocation::Terminal(_) => {}
        }

        let bounds_page = read_journal_page(&state, &flow_slug, &run_id, 0)
            .await
            .map_err(|error| journal_read_http_error(&run_id, error))?;
        let mut after_sequence = 0;
        let first_page = if let Some(cursor) = cursor.as_ref() {
            cursor
                .validate_against(&bounds_page.bounds)
                .map_err(cursor_error_response)?;
            after_sequence = cursor.sequence();
            let page = read_journal_page(&state, &flow_slug, &run_id, after_sequence)
                .await
                .map_err(|error| journal_read_http_error(&run_id, error))?;
            cursor
                .validate_against(&page.bounds)
                .map_err(cursor_error_response)?;
            page
        } else {
            bounds_page
        };

        let store = state.store.clone();
        let config = store.event_journal_config();
        let run_id_for_stream = run_id.clone();
        let flow_for_stream = flow_slug.clone();
        let acknowledged_through = cursor.as_ref().map(RunEventCursor::sequence);
        let stream = async_stream::stream! {
            let _sse_permit = sse_permit;
            let mut pending_page = Some(first_page);
            let mut consecutive_failures = 0u32;

            loop {
                let page = if let Some(page) = pending_page.take() {
                    page
                } else {
                    match tokio::time::timeout(
                        config.read_timeout,
                        store.read_run_events(
                            &flow_for_stream,
                            &run_id_for_stream,
                            after_sequence,
                        ),
                    )
                    .await
                    {
                        Ok(Ok(page)) => {
                            consecutive_failures = 0;
                            page
                        }
                        Ok(Err(error)) => {
                            consecutive_failures = consecutive_failures.saturating_add(1);
                            if consecutive_failures == 1 || consecutive_failures.is_power_of_two() {
                                tracing::warn!(
                                    run_id = %run_id_for_stream,
                                    consecutive_failures,
                                    %error,
                                    "Durable SSE journal read failed"
                                );
                            }
                            if consecutive_failures >= 5 {
                                yield Ok::<Event, Infallible>(Event::default().event("error").data(
                                    r#"{"event":"error","data":{"message":"run-event replay is temporarily unavailable; reconnect with Last-Event-ID"}}"#,
                                ));
                                return;
                            }
                            let factor = 1u32 << consecutive_failures.min(3);
                            tokio::time::sleep(config.poll_interval.saturating_mul(factor)).await;
                            continue;
                        }
                        Err(_) => {
                            consecutive_failures = consecutive_failures.saturating_add(1);
                            if consecutive_failures == 1 || consecutive_failures.is_power_of_two() {
                                tracing::warn!(
                                    run_id = %run_id_for_stream,
                                    consecutive_failures,
                                    timeout_ms = config.read_timeout.as_millis(),
                                    "Durable SSE journal read timed out"
                                );
                            }
                            if consecutive_failures >= 5 {
                                yield Ok::<Event, Infallible>(Event::default().event("error").data(
                                    r#"{"event":"error","data":{"message":"run-event replay timed out; reconnect with Last-Event-ID"}}"#,
                                ));
                                return;
                            }
                            let factor = 1u32 << consecutive_failures.min(3);
                            tokio::time::sleep(config.poll_interval.saturating_mul(factor)).await;
                            continue;
                        }
                    }
                };

                if let Some(gap) = page.gap.as_ref() {
                    after_sequence = after_sequence.max(gap.last_sequence);
                    let cursor_id = RunEventCursor::new(
                        run_id_for_stream.clone(),
                        gap.last_sequence,
                    )
                    .expect("validated journal gap must have a valid cursor")
                    .to_string();
                    let data = serde_json::json!({
                        "event": "journal_gap",
                        "data": gap,
                    });
                    yield Ok::<Event, Infallible>(Event::default()
                        .id(cursor_id)
                        .event("journal_gap")
                        .data(data.to_string()));
                }

                let mut delivered_terminal = false;
                for entry in &page.events {
                    after_sequence = entry.sequence;
                    let cursor_id = RunEventCursor::new(
                        run_id_for_stream.clone(),
                        entry.sequence,
                    )
                    .expect("validated journal event must have a valid cursor")
                    .to_string();
                    yield Ok::<Event, Infallible>(Event::default()
                        .id(cursor_id)
                        .event(entry.event_type.clone())
                        .data(entry.payload.to_string()));
                    if entry.event_type == "run_complete" {
                        delivered_terminal = true;
                        break;
                    }
                }
                if delivered_terminal {
                    return;
                }

                if let Some(terminal) = page.terminal.as_ref() {
                    match terminal.event_sequence {
                        Some(sequence) if after_sequence < sequence => {
                            // The bounded page has more retained events.
                            continue;
                        }
                        Some(sequence)
                            if cursor_acknowledges_terminal(acknowledged_through, sequence) =>
                        {
                            // The cursor supplied by this client explicitly
                            // acknowledged the terminal event (or a later
                            // sequence), so an empty closing replay is correct.
                            // Do not use the advancing server-side cursor here:
                            // a retention gap may have moved it past an event
                            // the client never actually received.
                            return;
                        }
                        Some(_) | None => {
                            // Physical retention or a best-effort writer
                            // failure removed/omitted run_complete. The durable
                            // run record is authoritative and explicitly marks
                            // this synthetic fallback as an incomplete journal.
                            let data = serde_json::json!({
                                "event": "run_complete",
                                "data": {
                                    "run_id": run_id_for_stream.clone(),
                                    "status": terminal.status.to_string(),
                                    "duration_ms": terminal.duration_ms,
                                    "total_tokens": terminal.total_tokens,
                                    "journal_complete": false,
                                    "synthesized_from_run_record": true,
                                }
                            });
                            yield Ok::<Event, Infallible>(Event::default()
                                .event("run_complete")
                                .data(data.to_string()));
                            return;
                        }
                    }
                }

                if after_sequence < page.bounds.latest_sequence {
                    continue;
                }
                tokio::time::sleep(config.poll_interval).await;
            }
        };

        let response = Sse::new(stream)
            .keep_alive(
                KeepAlive::new()
                    .interval(std::time::Duration::from_secs(15))
                    .text("keep-alive"),
            )
            .into_response();
        return Ok(hardened_sse_response(response));
    }

    if cursor.is_some() {
        return Err(structured_error(
            StatusCode::CONFLICT,
            "Last-Event-ID replay requires a shared run-event journal",
        ));
    }

    // Subscribe and snapshot under one EventBus critical section so an event
    // cannot land in the replay/subscription gap.
    let local_subscription = {
        let active_runs = state.active_runs.read().await;
        match active_runs.get(&run_id) {
            Some(active_run) if active_run.flow == flow_slug => {
                Some(active_run.eventbus.subscribe_with_replay())
            }
            Some(_) => {
                return Err(structured_error(
                    StatusCode::NOT_FOUND,
                    format!("Run '{}' not found or already completed", run_id),
                ));
            }
            None => None,
        }
    };
    let (replay, rx) = match local_subscription {
        Some((replay, rx)) => (replay, Some(rx)),
        None => match durable_run_location(&state, &flow_slug, &run_id)
            .await
            .map_err(run_location_store_error)?
        {
            DurableRunLocation::Terminal(record) => (
                vec![Arc::new(CrewEvent::RunComplete {
                    run_id: record.run_id,
                    status: record.status.to_string(),
                    duration_ms: record.duration_ms,
                    total_tokens: record.total_tokens,
                })],
                None,
            ),
            location => return Err(run_location_error(&state, &run_id, location)),
        },
    };

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
            let data = serde_json::to_string(ev).unwrap_or_default();
            yield Ok::<Event, Infallible>(Event::default().event(ev.event_type()).data(data));

            if matches!(ev, CrewEvent::RunComplete { .. }) {
                return; // Run already finished, no need for live stream
            }
        }

        let Some(mut rx) = rx else {
            return;
        };

        // Then: stream live events
        loop {
            match rx.recv().await {
                Ok(event) => {
                    let effective = maybe_truncate_event(&event, sse_max_chars);
                    let ev = effective.as_ref().unwrap_or(&event);
                    let data = serde_json::to_string(ev).unwrap_or_default();
                    yield Ok::<Event, Infallible>(Event::default().event(ev.event_type()).data(data));

                    if matches!(ev, CrewEvent::RunComplete { .. }) {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    let sse_event = Event::default()
                        .event("warning")
                        .data(format!("{{\"message\":\"missed {} events\"}}", n));
                    yield Ok::<Event, Infallible>(sse_event);
                }
                Err(broadcast::error::RecvError::Closed) => {
                    break;
                }
            }
        }
    };

    let response = Sse::new(stream)
        .keep_alive(
            KeepAlive::new()
                .interval(std::time::Duration::from_secs(15))
                .text("keep-alive"),
        )
        .into_response();
    Ok(hardened_sse_response(response))
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
        RunWorkResult, TERMINAL_RESULT_RETRY_RETAINED_BYTES, TerminalPersistence,
        TerminalResultRetention, classify_work_result, cursor_acknowledges_terminal,
        cursor_error_response, persist_terminal_outcome, replay_run, truncate_utf8,
        validate_run_id, validate_run_tags,
    };
    use crate::engine::idempotency::{
        IdempotencyRecord, IdempotencyState, PrincipalId, RUN_OPERATION,
    };
    use crate::engine::run_events::RunEventCursorError;
    use crate::engine::run_history::{JsonFileStore, RunCompletion, RunStatus};
    use crate::engine::store::StateStore;
    use crate::engine::task::TaskResult;
    use crate::utils::error::IronCrewError;
    use std::sync::Arc;

    #[test]
    fn ascii_under_limit_returns_full() {
        assert_eq!(truncate_utf8("hello", 10), "hello");
    }

    #[test]
    fn only_the_client_supplied_cursor_acknowledges_terminal_replay() {
        assert!(!cursor_acknowledges_terminal(None, 7));
        assert!(!cursor_acknowledges_terminal(Some(6), 7));
        assert!(cursor_acknowledges_terminal(Some(7), 7));
        assert!(cursor_acknowledges_terminal(Some(8), 7));
    }

    #[test]
    fn non_ascii_cursor_uses_the_structured_invalid_cursor_contract() {
        let (status, body) = cursor_error_response(RunEventCursorError::NonAscii);
        assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
        assert_eq!(body.0["code"], "invalid_cursor");
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

    #[test]
    fn legacy_run_replay_is_enriched_with_owner_metadata() {
        let record = IdempotencyRecord {
            key_hash: "a".repeat(64),
            principal_id: PrincipalId::legacy(),
            request_fingerprint: "b".repeat(64),
            operation: RUN_OPERATION.into(),
            scope: "legacy-flow".into(),
            resource_id: "legacy-run".into(),
            exclusive_scope: None,
            attempt_id: "attempt-1".into(),
            owner_instance_id: "owner-a".into(),
            base_revision: None,
            state: IdempotencyState::Running,
            response_status: Some(200),
            response_body: Some(r#"{"run_id":"legacy-run","status":"started"}"#.into()),
            lease_expires_at: "2026-07-19T12:01:00Z".into(),
            created_at: "2026-07-19T12:00:00Z".into(),
            updated_at: "2026-07-19T12:00:00Z".into(),
            completed_at: None,
            expires_at: None,
            ttl_seconds: 86_400,
        };

        let (_, axum::Json(body)) = match replay_run(&record) {
            Ok(response) => response,
            Err(_) => panic!("valid legacy response must replay"),
        };
        assert_eq!(body["owner_instance_id"], "owner-a");
        assert_eq!(body["control_scope"], "process");
    }

    fn result_completion(output_bytes: usize) -> RunCompletion {
        RunCompletion {
            status: RunStatus::Success,
            finished_at: "2026-07-19T00:00:00Z".into(),
            duration_ms: 123,
            task_results: vec![TaskResult {
                task: "task".into(),
                agent: "agent".into(),
                output: "x".repeat(output_bytes),
                success: true,
                duration_ms: 100,
                token_usage: None,
                reasoning: Some("reasoning".into()),
            }],
            total_tokens: 42,
            cached_tokens: 7,
        }
    }

    #[test]
    fn small_terminal_results_get_one_bounded_full_retry_then_release() {
        let mut retention = TerminalResultRetention::new(Some(result_completion(128)));

        assert!(retention.record_failure().is_none());
        assert_eq!(retention.completion().unwrap().task_results.len(), 1);

        let released = retention
            .record_failure()
            .expect("second failed full-payload attempt must release results");
        assert_eq!(released.result_count, 1);
        assert_eq!(released.full_failures, 2);

        let completion = retention.completion().unwrap();
        assert!(completion.task_results.is_empty());
        assert_eq!(completion.status, RunStatus::Success);
        assert_eq!(completion.duration_ms, 123);
        assert_eq!(completion.total_tokens, 42);
        assert_eq!(completion.cached_tokens, 7);
    }

    #[test]
    fn large_terminal_results_release_after_first_failed_write() {
        let mut retention = TerminalResultRetention::new(Some(result_completion(
            TERMINAL_RESULT_RETRY_RETAINED_BYTES + 1,
        )));

        let released = retention
            .record_failure()
            .expect("oversized retry payload must be released immediately");
        assert_eq!(released.result_count, 1);
        assert_eq!(released.full_failures, 1);
        assert!(released.retained_bytes > TERMINAL_RESULT_RETRY_RETAINED_BYTES);
        assert!(retention.completion().unwrap().task_results.is_empty());
    }

    #[tokio::test]
    async fn panicked_work_is_persisted_as_failed() {
        let handle: tokio::task::JoinHandle<std::result::Result<RunWorkResult, IronCrewError>> =
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
                completion: None,
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
