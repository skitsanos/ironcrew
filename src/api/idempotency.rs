//! HTTP idempotency-key parsing, request fingerprints, and bounded response
//! serialization. Raw client keys are deliberately discarded after hashing.

use std::io::{self, Write};
use std::sync::Arc;
use std::time::Duration;

use axum::http::{HeaderMap, HeaderName, HeaderValue};
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::utils::error::{IronCrewError, Result};

use crate::engine::idempotency::{
    HARD_IDEMPOTENCY_RESPONSE_BYTES, IdempotencyLimits, PrincipalId, RunFenceHeartbeat,
};
use crate::engine::store::StateStore;

pub const IDEMPOTENCY_KEY_HEADER: HeaderName = HeaderName::from_static("idempotency-key");
pub const IDEMPOTENCY_RECOVERY_KEY_HEADER: HeaderName =
    HeaderName::from_static("idempotency-recovery-key");
pub const IDEMPOTENCY_REPLAYED_HEADER: HeaderName = HeaderName::from_static("idempotency-replayed");

const DEFAULT_TTL_SECONDS: u64 = 24 * 60 * 60;
const MIN_TTL_SECONDS: u64 = 60;
const MAX_TTL_SECONDS: u64 = 30 * 24 * 60 * 60;
const DEFAULT_MAX_RECORDS: usize = 10_000;
const HARD_MAX_RECORDS: usize = 100_000;
const DEFAULT_PRUNE_BATCH: usize = 1_000;
const HARD_PRUNE_BATCH: usize = 10_000;
const DEFAULT_MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
const DEFAULT_MAX_TOTAL_RESPONSE_BYTES: usize = 256 * 1024 * 1024;
const HARD_MAX_TOTAL_RESPONSE_BYTES: usize = 8 * 1024 * 1024 * 1024;

/// Begin the process-local fence at the storage invocation, never when its
/// response arrives. Every backend commits a deadline sampled at or after this
/// point, so this monotonic deadline cannot outlive the durable renewal.
pub(crate) fn conservative_lease_deadline(
    heartbeat_started: tokio::time::Instant,
    lease_ttl: Duration,
) -> tokio::time::Instant {
    heartbeat_started + lease_ttl
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HeartbeatWake {
    LeaseExpired,
    Tick,
}

async fn wait_for_heartbeat_or_expiry(
    interval: &mut tokio::time::Interval,
    lease_deadline: tokio::time::Instant,
) -> HeartbeatWake {
    tokio::select! {
        biased;
        _ = tokio::time::sleep_until(lease_deadline) => HeartbeatWake::LeaseExpired,
        _ = interval.tick() => HeartbeatWake::Tick,
    }
}

/// PostgreSQL heartbeats install an earlier database timeout and may be
/// cancelled at the local fence. Local stores finish their owned synchronous
/// or blocking work first, then reject a result that arrived after expiry so a
/// dropped `spawn_blocking` handle cannot leave an orphaned transaction.
async fn heartbeat_before_deadline<T, F>(
    cancellable: bool,
    lease_deadline: tokio::time::Instant,
    future: F,
) -> Option<Result<T>>
where
    F: std::future::Future<Output = Result<T>>,
{
    if cancellable {
        tokio::time::timeout_at(lease_deadline, future).await.ok()
    } else {
        let result = future.await;
        (tokio::time::Instant::now() < lease_deadline).then_some(result)
    }
}

/// A fenced lease heartbeat whose worker cannot outlive the request task that
/// owns it. Consumers select on `loss_receiver()` and stop external work as
/// soon as another attempt owns the claim, or once storage errors have lasted
/// through the complete local lease window.
pub struct LeaseHeartbeat {
    task: tokio::task::JoinHandle<()>,
    loss: tokio::sync::watch::Receiver<bool>,
}

impl LeaseHeartbeat {
    pub fn spawn(
        store: Arc<dyn StateStore>,
        key_hash: String,
        attempt_id: String,
        operation: &'static str,
        initial_lease_deadline: tokio::time::Instant,
    ) -> Self {
        let (loss_tx, loss) = tokio::sync::watch::channel(false);
        let task = tokio::spawn(async move {
            let lease_ttl = store.run_lease_ttl();
            let heartbeat_every = crate::engine::store::run_lease_heartbeat_interval(lease_ttl);
            let mut lease_deadline = initial_lease_deadline;
            let mut interval = tokio::time::interval(heartbeat_every);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

            loop {
                if wait_for_heartbeat_or_expiry(&mut interval, lease_deadline).await
                    == HeartbeatWake::LeaseExpired
                {
                    tracing::warn!(
                        operation,
                        "Idempotency lease expired before the next heartbeat completed"
                    );
                    crate::metrics::record_lease_loss(crate::metrics::LeaseScope::Conversation);
                    let _ = loss_tx.send(true);
                    return;
                }
                let heartbeat_started = tokio::time::Instant::now();
                let deadline = chrono::Utc::now()
                    .checked_add_signed(
                        chrono::Duration::from_std(lease_ttl)
                            .unwrap_or_else(|_| chrono::Duration::seconds(60)),
                    )
                    .unwrap_or(chrono::DateTime::<chrono::Utc>::MAX_UTC)
                    .to_rfc3339();
                match heartbeat_before_deadline(
                    store.run_maintenance_watchdog().is_some(),
                    lease_deadline,
                    store.heartbeat_idempotency(&key_hash, &attempt_id, &deadline),
                )
                .await
                {
                    Some(Ok(true)) => {
                        lease_deadline = conservative_lease_deadline(heartbeat_started, lease_ttl);
                    }
                    Some(Ok(false)) | Some(Err(IronCrewError::Conflict(_))) => {
                        tracing::warn!(operation, "Idempotency claim was fenced during execution");
                        crate::metrics::record_lease_loss(crate::metrics::LeaseScope::Conversation);
                        let _ = loss_tx.send(true);
                        return;
                    }
                    Some(Err(error)) => {
                        crate::metrics::record_store_failure(
                            crate::metrics::StoreOperation::LeaseHeartbeat,
                        );
                        tracing::error!(operation, %error, "Failed to heartbeat idempotency claim");
                        if tokio::time::Instant::now() >= lease_deadline {
                            tracing::warn!(
                                operation,
                                "Idempotency storage remained unavailable through the lease deadline"
                            );
                            crate::metrics::record_lease_loss(
                                crate::metrics::LeaseScope::Conversation,
                            );
                            let _ = loss_tx.send(true);
                            return;
                        }
                    }
                    None => {
                        crate::metrics::record_store_failure(
                            crate::metrics::StoreOperation::LeaseHeartbeat,
                        );
                        tracing::warn!(
                            operation,
                            "Idempotency heartbeat exceeded the remaining lease window"
                        );
                        crate::metrics::record_lease_loss(crate::metrics::LeaseScope::Conversation);
                        let _ = loss_tx.send(true);
                        return;
                    }
                }
            }
        });
        Self { task, loss }
    }

    pub fn loss_receiver(&self) -> tokio::sync::watch::Receiver<bool> {
        self.loss.clone()
    }
}

impl Drop for LeaseHeartbeat {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LeaseLossSource {
    Heartbeat,
    ClosedChannel,
}

async fn lease_loss_source(loss: &mut tokio::sync::watch::Receiver<bool>) -> LeaseLossSource {
    if *loss.borrow() {
        return LeaseLossSource::Heartbeat;
    }
    match loss.wait_for(|lost| *lost).await {
        Ok(_) => LeaseLossSource::Heartbeat,
        Err(_) => LeaseLossSource::ClosedChannel,
    }
}

pub async fn wait_for_lease_loss(loss: &mut tokio::sync::watch::Receiver<bool>) {
    // A closed channel means the heartbeat worker exited unexpectedly. Treat
    // that exactly like a lost fence; continuing side effects would be unsafe.
    if lease_loss_source(loss).await == LeaseLossSource::ClosedChannel {
        crate::metrics::record_lease_loss(crate::metrics::LeaseScope::Conversation);
    }
}

/// Heartbeat for an idempotent run. Unlike a conversation operation, a run
/// owns two durable fences: the operation ledger and the run record itself.
/// Backends renew and validate both atomically so the global run reconciler
/// cannot terminalize a run while its Lua worker continues side effects.
pub struct RunLeaseHeartbeat {
    task: tokio::task::JoinHandle<()>,
    outcome: tokio::sync::watch::Receiver<Option<RunFenceHeartbeat>>,
    coordinator: Arc<RunFenceCoordinator>,
}

struct RunFenceState {
    lease_deadline: tokio::time::Instant,
    outcome: Option<RunFenceHeartbeat>,
}

struct RunFenceCoordinator {
    state: std::sync::Mutex<RunFenceState>,
    outcome_tx: tokio::sync::watch::Sender<Option<RunFenceHeartbeat>>,
}

impl RunFenceCoordinator {
    fn new(
        lease_deadline: tokio::time::Instant,
    ) -> (
        Arc<Self>,
        tokio::sync::watch::Receiver<Option<RunFenceHeartbeat>>,
    ) {
        let (outcome_tx, outcome) = tokio::sync::watch::channel(None);
        (
            Arc::new(Self {
                state: std::sync::Mutex::new(RunFenceState {
                    lease_deadline,
                    outcome: None,
                }),
                outcome_tx,
            }),
            outcome,
        )
    }

    fn state(&self) -> std::sync::MutexGuard<'_, RunFenceState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn current_deadline(&self) -> tokio::time::Instant {
        self.state().lease_deadline
    }

    fn publish_locked(&self, state: &mut RunFenceState, outcome: RunFenceHeartbeat) {
        if state.outcome.is_some() {
            return;
        }
        state.outcome = Some(outcome.clone());
        let _ = self.outcome_tx.send(Some(outcome));
    }

    fn publish(&self, outcome: RunFenceHeartbeat) {
        let mut state = self.state();
        self.publish_locked(&mut state, outcome);
    }

    fn lose_if_expired(&self, expected_deadline: tokio::time::Instant) -> bool {
        let mut state = self.state();
        if state.outcome.is_some() {
            return true;
        }
        if state.lease_deadline != expected_deadline
            || tokio::time::Instant::now() < state.lease_deadline
        {
            return false;
        }
        self.publish_locked(&mut state, RunFenceHeartbeat::Lost);
        true
    }

    fn renew_if_live(
        &self,
        expected_deadline: tokio::time::Instant,
        renewed_deadline: tokio::time::Instant,
    ) -> bool {
        let mut state = self.state();
        let now = tokio::time::Instant::now();
        if state.outcome.is_some() {
            return false;
        }
        if state.lease_deadline != expected_deadline
            || now >= state.lease_deadline
            || now >= renewed_deadline
        {
            self.publish_locked(&mut state, RunFenceHeartbeat::Lost);
            return false;
        }
        state.lease_deadline = renewed_deadline;
        true
    }

    /// Release the keyed worker only while holding the same lock used to
    /// publish loss and renew the conservative local deadline. Returning an
    /// outcome leaves the oneshot sender unopened, so the worker body cannot
    /// be polled after an already-observed fence loss.
    fn admit_execution(
        &self,
        start: tokio::sync::oneshot::Sender<()>,
    ) -> Option<RunFenceHeartbeat> {
        let mut state = self.state();
        if let Some(outcome) = state.outcome.clone() {
            return Some(outcome);
        }
        if tokio::time::Instant::now() >= state.lease_deadline {
            self.publish_locked(&mut state, RunFenceHeartbeat::Lost);
            return Some(RunFenceHeartbeat::Lost);
        }
        // `send` is synchronous. A concurrent heartbeat cannot publish loss
        // between this live-fence check and opening the worker gate.
        let _ = start.send(());
        None
    }
}

/// The shared watch sender must remain available for synchronous admission.
/// This guard preserves the fail-closed channel contract if the heartbeat
/// task is cancelled or panics before publishing its own outcome.
struct RunFenceTaskGuard {
    coordinator: Arc<RunFenceCoordinator>,
}

impl Drop for RunFenceTaskGuard {
    fn drop(&mut self) {
        self.coordinator.publish(RunFenceHeartbeat::Lost);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InitialRunHeartbeatMetric {
    None,
    LeaseLoss,
    StoreFailure,
}

fn initial_run_heartbeat_metric(
    established: &Option<Result<RunFenceHeartbeat>>,
) -> InitialRunHeartbeatMetric {
    match established {
        Some(Ok(RunFenceHeartbeat::Lost)) | Some(Err(IronCrewError::Conflict(_))) => {
            InitialRunHeartbeatMetric::LeaseLoss
        }
        Some(Err(_)) | None => InitialRunHeartbeatMetric::StoreFailure,
        Some(Ok(_)) => InitialRunHeartbeatMetric::None,
    }
}

fn record_initial_run_heartbeat_metric(established: &Option<Result<RunFenceHeartbeat>>) {
    match initial_run_heartbeat_metric(established) {
        InitialRunHeartbeatMetric::None => {}
        InitialRunHeartbeatMetric::LeaseLoss => {
            crate::metrics::record_lease_loss(crate::metrics::LeaseScope::Run);
        }
        InitialRunHeartbeatMetric::StoreFailure => {
            crate::metrics::record_store_failure(crate::metrics::StoreOperation::LeaseHeartbeat);
        }
    }
}

impl RunLeaseHeartbeat {
    /// Confirm the claimed run fence before any Lua work is admitted, then
    /// keep it alive in the background. A claimed ledger without a run row is
    /// a valid pre-execution fence; the subsequent run-intent transaction
    /// atomically advances that same claim to `running`.
    pub async fn start(
        store: Arc<dyn StateStore>,
        run_id: String,
        key_hash: String,
        attempt_id: String,
        initial_lease_deadline: tokio::time::Instant,
    ) -> Result<Self> {
        let lease_ttl = store.run_lease_ttl();
        let heartbeat_started = tokio::time::Instant::now();
        let deadline = chrono::Utc::now()
            .checked_add_signed(
                chrono::Duration::from_std(lease_ttl)
                    .unwrap_or_else(|_| chrono::Duration::seconds(60)),
            )
            .unwrap_or(chrono::DateTime::<chrono::Utc>::MAX_UTC)
            .to_rfc3339();
        let established = heartbeat_before_deadline(
            store.run_maintenance_watchdog().is_some(),
            initial_lease_deadline,
            store.heartbeat_idempotent_run(&run_id, &key_hash, &attempt_id, &deadline),
        )
        .await;
        record_initial_run_heartbeat_metric(&established);
        let established = match established {
            Some(Ok(established)) => established,
            Some(Err(error)) => return Err(error),
            None => {
                return Err(IronCrewError::Validation(
                    "The initial run-fence heartbeat exceeded the remaining lease window".into(),
                ));
            }
        };
        match established {
            RunFenceHeartbeat::Owned => {}
            RunFenceHeartbeat::CancelRequested => {
                return Err(IronCrewError::Conflict(
                    "Run cancellation was requested before execution started".into(),
                ));
            }
            RunFenceHeartbeat::Terminal(status) => {
                return Err(IronCrewError::Conflict(format!(
                    "Run became {status} before execution started"
                )));
            }
            RunFenceHeartbeat::Lost => {
                return Err(IronCrewError::Conflict(
                    "The durable run fence was lost before execution started".into(),
                ));
            }
        }

        let established_deadline = conservative_lease_deadline(heartbeat_started, lease_ttl);
        Ok(Self::spawn_loop(
            store,
            run_id,
            key_hash,
            attempt_id,
            established_deadline,
        ))
    }

    fn spawn_loop(
        store: Arc<dyn StateStore>,
        run_id: String,
        key_hash: String,
        attempt_id: String,
        initial_lease_deadline: tokio::time::Instant,
    ) -> Self {
        let (coordinator, outcome) = RunFenceCoordinator::new(initial_lease_deadline);
        let task_coordinator = coordinator.clone();
        let task_guard = RunFenceTaskGuard {
            coordinator: coordinator.clone(),
        };
        let task = tokio::spawn(async move {
            let _task_guard = task_guard;
            let lease_ttl = store.run_lease_ttl();
            let heartbeat_every = crate::engine::store::run_lease_heartbeat_interval(lease_ttl);
            let mut interval = tokio::time::interval_at(
                tokio::time::Instant::now() + heartbeat_every,
                heartbeat_every,
            );
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

            loop {
                let lease_deadline = task_coordinator.current_deadline();
                if wait_for_heartbeat_or_expiry(&mut interval, lease_deadline).await
                    == HeartbeatWake::LeaseExpired
                {
                    if task_coordinator.lose_if_expired(lease_deadline) {
                        tracing::warn!(
                            run_id,
                            "Run fence expired before the next heartbeat completed"
                        );
                        return;
                    }
                    continue;
                }
                let heartbeat_started = tokio::time::Instant::now();
                let deadline = chrono::Utc::now()
                    .checked_add_signed(
                        chrono::Duration::from_std(lease_ttl)
                            .unwrap_or_else(|_| chrono::Duration::seconds(60)),
                    )
                    .unwrap_or(chrono::DateTime::<chrono::Utc>::MAX_UTC)
                    .to_rfc3339();
                match heartbeat_before_deadline(
                    store.run_maintenance_watchdog().is_some(),
                    lease_deadline,
                    store.heartbeat_idempotent_run(&run_id, &key_hash, &attempt_id, &deadline),
                )
                .await
                {
                    Some(Ok(RunFenceHeartbeat::Owned)) => {
                        let renewed_deadline =
                            conservative_lease_deadline(heartbeat_started, lease_ttl);
                        if !task_coordinator.renew_if_live(lease_deadline, renewed_deadline) {
                            tracing::warn!(
                                run_id,
                                "Run fence expired while its heartbeat response was being admitted"
                            );
                            return;
                        }
                    }
                    Some(Ok(RunFenceHeartbeat::CancelRequested)) => {
                        tracing::info!(run_id, "Durable run cancellation was requested");
                        task_coordinator.publish(RunFenceHeartbeat::CancelRequested);
                        return;
                    }
                    Some(Ok(outcome @ RunFenceHeartbeat::Terminal(_))) => {
                        tracing::debug!(run_id, "Run heartbeat observed a terminal run fence");
                        task_coordinator.publish(outcome);
                        return;
                    }
                    Some(Ok(RunFenceHeartbeat::Lost)) | Some(Err(IronCrewError::Conflict(_))) => {
                        tracing::warn!(run_id, "Idempotent run fence was lost during execution");
                        task_coordinator.publish(RunFenceHeartbeat::Lost);
                        return;
                    }
                    Some(Err(error)) => {
                        crate::metrics::record_store_failure(
                            crate::metrics::StoreOperation::LeaseHeartbeat,
                        );
                        tracing::error!(run_id, %error, "Failed to heartbeat idempotent run fence");
                        if task_coordinator.lose_if_expired(lease_deadline) {
                            tracing::warn!(
                                run_id,
                                "Run-fence storage remained unavailable through the lease deadline"
                            );
                            return;
                        }
                    }
                    None => {
                        crate::metrics::record_store_failure(
                            crate::metrics::StoreOperation::LeaseHeartbeat,
                        );
                        tracing::warn!(
                            run_id,
                            "Run-fence heartbeat exceeded the remaining lease window"
                        );
                        task_coordinator.publish(RunFenceHeartbeat::Lost);
                        return;
                    }
                }
            }
        });
        Self {
            task,
            outcome,
            coordinator,
        }
    }

    pub fn outcome_receiver(&self) -> tokio::sync::watch::Receiver<Option<RunFenceHeartbeat>> {
        self.outcome.clone()
    }

    /// Atomically admit a keyed worker against the latest conservative local
    /// deadline, or return the fence outcome that must be finalized instead.
    pub(crate) fn admit_execution(
        &self,
        start: tokio::sync::oneshot::Sender<()>,
    ) -> Option<RunFenceHeartbeat> {
        self.coordinator.admit_execution(start)
    }

    #[cfg(test)]
    pub(crate) fn for_test_with_deadline(lease_deadline: tokio::time::Instant) -> Self {
        let (coordinator, outcome) = RunFenceCoordinator::new(lease_deadline);
        let task_guard = RunFenceTaskGuard {
            coordinator: coordinator.clone(),
        };
        let task = tokio::spawn(async move {
            let _task_guard = task_guard;
            std::future::pending::<()>().await;
        });
        Self {
            task,
            outcome,
            coordinator,
        }
    }
}

impl Drop for RunLeaseHeartbeat {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub async fn wait_for_run_fence_outcome(
    outcome: &mut tokio::sync::watch::Receiver<Option<RunFenceHeartbeat>>,
) -> RunFenceHeartbeat {
    if let Some(outcome) = outcome.borrow().clone() {
        return outcome;
    }
    match outcome.wait_for(Option::is_some).await {
        Ok(value) => value.clone().unwrap_or(RunFenceHeartbeat::Lost),
        Err(_) => RunFenceHeartbeat::Lost,
    }
}

#[derive(Debug, Clone)]
pub struct IdempotencyConfig {
    pub require_key: bool,
    pub ttl_seconds: u64,
    pub max_records: usize,
    pub max_records_per_principal: usize,
    pub max_in_flight_per_principal: usize,
    pub prune_batch: usize,
    pub max_response_bytes: usize,
    pub max_total_response_bytes: usize,
    pub max_total_response_bytes_per_principal: usize,
}

impl IdempotencyConfig {
    /// Parse and validate the complete idempotency resource policy once at
    /// process startup. Retention cannot be shorter than the longest admitted
    /// run plus one hour, otherwise a retry could outlive its ledger row.
    pub fn from_env(max_run_lifetime: Duration) -> Result<Self> {
        let require_key = bool_env("IRONCREW_REQUIRE_IDEMPOTENCY_KEY", false)?;
        let configured_ttl = bounded_env_u64(
            "IRONCREW_IDEMPOTENCY_TTL_SECONDS",
            DEFAULT_TTL_SECONDS,
            MIN_TTL_SECONDS,
            MAX_TTL_SECONDS,
        )?;
        let minimum_safe_ttl = max_run_lifetime
            .as_secs()
            .saturating_add(60 * 60)
            .clamp(MIN_TTL_SECONDS, MAX_TTL_SECONDS);
        if configured_ttl < minimum_safe_ttl {
            return Err(IronCrewError::Validation(format!(
                "IRONCREW_IDEMPOTENCY_TTL_SECONDS must be at least {minimum_safe_ttl} for the configured run lifetime"
            )));
        }

        let max_records = bounded_env_usize(
            "IRONCREW_IDEMPOTENCY_MAX_RECORDS",
            DEFAULT_MAX_RECORDS,
            1,
            HARD_MAX_RECORDS,
        )?;
        let max_total_response_bytes = bounded_env_usize(
            "IRONCREW_IDEMPOTENCY_MAX_TOTAL_RESPONSE_BYTES",
            DEFAULT_MAX_TOTAL_RESPONSE_BYTES,
            1,
            HARD_MAX_TOTAL_RESPONSE_BYTES,
        )?;
        let config = Self {
            require_key,
            ttl_seconds: configured_ttl,
            max_records,
            max_records_per_principal: bounded_env_usize(
                "IRONCREW_IDEMPOTENCY_MAX_RECORDS_PER_PRINCIPAL",
                max_records,
                1,
                max_records,
            )?,
            max_in_flight_per_principal: bounded_env_usize(
                "IRONCREW_IDEMPOTENCY_MAX_IN_FLIGHT_PER_PRINCIPAL",
                max_records.min(64),
                1,
                max_records,
            )?,
            prune_batch: bounded_env_usize(
                "IRONCREW_IDEMPOTENCY_PRUNE_BATCH",
                DEFAULT_PRUNE_BATCH,
                1,
                HARD_PRUNE_BATCH,
            )?,
            max_response_bytes: bounded_env_usize(
                "IRONCREW_IDEMPOTENCY_MAX_RESPONSE_BYTES",
                DEFAULT_MAX_RESPONSE_BYTES,
                1,
                HARD_IDEMPOTENCY_RESPONSE_BYTES,
            )?,
            max_total_response_bytes,
            max_total_response_bytes_per_principal: bounded_env_usize(
                "IRONCREW_IDEMPOTENCY_MAX_TOTAL_RESPONSE_BYTES_PER_PRINCIPAL",
                max_total_response_bytes,
                1,
                max_total_response_bytes,
            )?,
        };
        config.limits().validate()?;
        Ok(config)
    }

    pub fn retention_expiry(&self, completed_at: chrono::DateTime<chrono::Utc>) -> String {
        completed_at
            .checked_add_signed(chrono::Duration::seconds(self.ttl_seconds as i64))
            .unwrap_or(chrono::DateTime::<chrono::Utc>::MAX_UTC)
            .to_rfc3339()
    }

    pub fn limits(&self) -> IdempotencyLimits {
        IdempotencyLimits {
            global_max_records: self.max_records,
            principal_max_records: self.max_records_per_principal,
            principal_max_in_flight: self.max_in_flight_per_principal,
            global_max_response_bytes: self.max_total_response_bytes,
            principal_max_response_bytes: self.max_total_response_bytes_per_principal,
            prune_batch: self.prune_batch,
        }
    }
}

impl Default for IdempotencyConfig {
    fn default() -> Self {
        Self {
            require_key: false,
            ttl_seconds: DEFAULT_TTL_SECONDS,
            max_records: DEFAULT_MAX_RECORDS,
            max_records_per_principal: DEFAULT_MAX_RECORDS,
            max_in_flight_per_principal: 64,
            prune_batch: DEFAULT_PRUNE_BATCH,
            max_response_bytes: DEFAULT_MAX_RESPONSE_BYTES,
            max_total_response_bytes: DEFAULT_MAX_TOTAL_RESPONSE_BYTES,
            max_total_response_bytes_per_principal: DEFAULT_MAX_TOTAL_RESPONSE_BYTES,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestKey {
    pub key_hash: String,
}

/// Parse exactly zero or one idempotency-key header. Only visible non-space
/// ASCII is accepted so proxies and databases cannot disagree about the key.
/// The returned value contains the SHA-256 digest only.
pub fn request_key(
    headers: &HeaderMap,
    required: bool,
    principal_id: &PrincipalId,
) -> Result<Option<RequestKey>> {
    parse_key_header(
        headers,
        &IDEMPOTENCY_KEY_HEADER,
        "Idempotency-Key",
        required,
        principal_id,
    )
}

pub fn recovery_key(headers: &HeaderMap, principal_id: &PrincipalId) -> Result<Option<RequestKey>> {
    parse_key_header(
        headers,
        &IDEMPOTENCY_RECOVERY_KEY_HEADER,
        "Idempotency-Recovery-Key",
        false,
        principal_id,
    )
}

fn parse_key_header(
    headers: &HeaderMap,
    header: &HeaderName,
    label: &str,
    required: bool,
    principal_id: &PrincipalId,
) -> Result<Option<RequestKey>> {
    let mut values = headers.get_all(header).iter();
    let Some(value) = values.next() else {
        if required {
            return Err(IronCrewError::Validation(format!(
                "{label} is required for this endpoint"
            )));
        }
        return Ok(None);
    };
    if values.next().is_some() {
        return Err(IronCrewError::Validation(format!(
            "Exactly one {label} header is allowed"
        )));
    }
    let bytes = value.as_bytes();
    if bytes.is_empty() || bytes.len() > 128 || !bytes.iter().all(|byte| (33..=126).contains(byte))
    {
        return Err(IronCrewError::Validation(format!(
            "{label} must be 1-128 visible ASCII bytes without whitespace"
        )));
    }
    // Legacy/anonymous deployments retain the original digest so an upgrade
    // cannot accidentally re-execute an existing key. Explicit named
    // principals receive separate namespaces and may safely reuse client keys.
    let key_hash = if principal_id == &PrincipalId::legacy() {
        hex_digest(bytes)
    } else {
        let mut encoder = FingerprintEncoder::new(b"ironcrew:idempotency-key:v2");
        encoder.field(principal_id.as_str().as_bytes());
        encoder.field(bytes);
        encoder.finish()
    };
    Ok(Some(RequestKey { key_hash }))
}

pub fn replay_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        IDEMPOTENCY_REPLAYED_HEADER,
        HeaderValue::from_static("true"),
    );
    headers
}

/// Versioned semantic fingerprint for `POST /flows/{flow}/run`. A missing
/// JSON body and explicit JSON `null` remain distinct.
pub fn run_fingerprint(flow: &str, body: Option<&Value>) -> String {
    let mut encoder = FingerprintEncoder::new(b"ironcrew:flow.run:v1");
    encoder.field(flow.as_bytes());
    match body {
        Some(value) => {
            encoder.field(b"body:present");
            encoder.json(value);
        }
        None => encoder.field(b"body:missing"),
    }
    encoder.finish()
}

/// Versioned fingerprint for a conversation message. `images = null`, an
/// absent field, and an empty array are deliberately equivalent because the
/// handler treats all three as a text-only turn.
pub fn conversation_message_fingerprint(
    flow: &str,
    conversation_id: &str,
    incarnation_id: &str,
    content: &str,
    images: Option<&[String]>,
) -> String {
    let mut encoder = FingerprintEncoder::new(b"ironcrew:conversation.message:v2");
    encoder.field(flow.as_bytes());
    encoder.field(conversation_id.as_bytes());
    encoder.field(incarnation_id.as_bytes());
    encoder.field(content.as_bytes());
    let images = images.unwrap_or_default();
    encoder.field(&(images.len() as u64).to_be_bytes());
    for image in images {
        encoder.field(image.as_bytes());
    }
    encoder.finish()
}

pub fn run_scope(flow: &str) -> String {
    flow.to_string()
}

pub fn conversation_scope(flow: &str, conversation_id: &str, incarnation_id: &str) -> String {
    crate::engine::sessions::conversation_mutation_scope(flow, conversation_id, incarnation_id)
}

/// Compactly serialize a response without ever allocating more than the
/// configured per-record limit. `None` is a durable non-replayable tombstone.
pub fn bounded_response_json<T: Serialize>(value: &T, max_bytes: usize) -> Result<Option<String>> {
    let writer = BoundedWriter::new(max_bytes);
    let mut serializer = serde_json::Serializer::new(writer);
    match value.serialize(&mut serializer) {
        Ok(()) => {
            let bytes = serializer.into_inner().bytes;
            String::from_utf8(bytes).map(Some).map_err(|error| {
                IronCrewError::Validation(format!(
                    "Serialized idempotency response was not UTF-8: {error}"
                ))
            })
        }
        Err(error) if error.is_io() => Ok(None),
        Err(error) => Err(IronCrewError::Validation(format!(
            "Failed to serialize idempotency response: {error}"
        ))),
    }
}

fn bool_env(name: &str, default: bool) -> Result<bool> {
    match std::env::var(name) {
        Err(std::env::VarError::NotPresent) => Ok(default),
        Ok(value) if value == "1" || value.eq_ignore_ascii_case("true") => Ok(true),
        Ok(value) if value == "0" || value.eq_ignore_ascii_case("false") => Ok(false),
        Ok(_) | Err(std::env::VarError::NotUnicode(_)) => Err(IronCrewError::Validation(format!(
            "{name} must be one of: 1, true, 0, false"
        ))),
    }
}

fn bounded_env_u64(name: &str, default: u64, min: u64, max: u64) -> Result<u64> {
    let value = match std::env::var(name) {
        Ok(value) => value.parse::<u64>().map_err(|_| {
            IronCrewError::Validation(format!("{name} must be an integer between {min} and {max}"))
        })?,
        Err(std::env::VarError::NotPresent) => default,
        Err(std::env::VarError::NotUnicode(_)) => {
            return Err(IronCrewError::Validation(format!(
                "{name} must contain valid UTF-8"
            )));
        }
    };
    if !(min..=max).contains(&value) {
        return Err(IronCrewError::Validation(format!(
            "{name} must be between {min} and {max}"
        )));
    }
    Ok(value)
}

fn bounded_env_usize(name: &str, default: usize, min: usize, max: usize) -> Result<usize> {
    let value = bounded_env_u64(name, default as u64, min as u64, max as u64)?;
    usize::try_from(value)
        .map_err(|_| IronCrewError::Validation(format!("{name} does not fit this platform")))
}

fn hex_digest(bytes: &[u8]) -> String {
    encode_hex(&Sha256::digest(bytes))
}

fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0x0f) as usize] as char);
    }
    encoded
}

struct FingerprintEncoder(Sha256);

impl FingerprintEncoder {
    fn new(domain: &[u8]) -> Self {
        let mut digest = Sha256::new();
        digest.update((domain.len() as u64).to_be_bytes());
        digest.update(domain);
        Self(digest)
    }

    fn field(&mut self, value: &[u8]) {
        self.0.update((value.len() as u64).to_be_bytes());
        self.0.update(value);
    }

    fn json(&mut self, value: &Value) {
        match value {
            Value::Null => self.field(b"null"),
            Value::Bool(value) => self.field(if *value { b"true" } else { b"false" }),
            Value::Number(value) => {
                self.field(b"number");
                self.field(value.to_string().as_bytes());
            }
            Value::String(value) => {
                self.field(b"string");
                self.field(value.as_bytes());
            }
            Value::Array(values) => {
                self.field(b"array");
                self.field(&(values.len() as u64).to_be_bytes());
                for value in values {
                    self.json(value);
                }
            }
            Value::Object(values) => {
                self.field(b"object");
                self.field(&(values.len() as u64).to_be_bytes());
                let mut keys = values.keys().collect::<Vec<_>>();
                keys.sort_unstable();
                for key in keys {
                    self.field(key.as_bytes());
                    self.json(&values[key]);
                }
            }
        }
    }

    fn finish(self) -> String {
        encode_hex(&self.0.finalize())
    }
}

struct BoundedWriter {
    bytes: Vec<u8>,
    limit: usize,
}

impl BoundedWriter {
    fn new(limit: usize) -> Self {
        Self {
            bytes: Vec::with_capacity(limit.min(64 * 1024)),
            limit,
        }
    }
}

impl Write for BoundedWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let new_len = self
            .bytes
            .len()
            .checked_add(buffer.len())
            .ok_or_else(|| io::Error::other("idempotency response exceeded its byte cap"))?;
        if new_len > self.limit {
            return Err(io::Error::other(
                "idempotency response exceeded its byte cap",
            ));
        }
        self.bytes.extend_from_slice(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn global_lease_losses(scope: &str) -> u64 {
        let mut body = String::new();
        crate::metrics::append_prometheus(&mut body);
        let prefix = format!("ironcrew_lease_losses_total{{scope=\"{scope}\"}} ");
        body.lines()
            .find_map(|line| line.strip_prefix(&prefix))
            .expect("fixed lease-loss series is rendered")
            .parse()
            .expect("lease-loss value is numeric")
    }

    #[test]
    fn request_key_is_hashed_and_malformed_or_multiple_values_fail() {
        let principal = PrincipalId::legacy();
        let mut headers = HeaderMap::new();
        headers.insert(
            IDEMPOTENCY_KEY_HEADER,
            HeaderValue::from_static("client-key-1"),
        );
        let parsed = request_key(&headers, true, &principal).unwrap().unwrap();
        assert_eq!(parsed.key_hash.len(), 64);
        assert!(!parsed.key_hash.contains("client-key-1"));

        headers.append(IDEMPOTENCY_KEY_HEADER, HeaderValue::from_static("second"));
        assert!(request_key(&headers, false, &principal).is_err());

        let mut headers = HeaderMap::new();
        headers.insert(
            IDEMPOTENCY_KEY_HEADER,
            HeaderValue::from_static("contains space"),
        );
        assert!(request_key(&headers, false, &principal).is_err());
        assert!(request_key(&HeaderMap::new(), true, &principal).is_err());
    }

    #[test]
    fn recovery_key_uses_the_same_secret_safe_validation() {
        let principal = PrincipalId::legacy();
        let mut headers = HeaderMap::new();
        headers.insert(
            IDEMPOTENCY_RECOVERY_KEY_HEADER,
            HeaderValue::from_static("prior-message-key"),
        );
        let parsed = recovery_key(&headers, &principal).unwrap().unwrap();
        assert_eq!(parsed.key_hash.len(), 64);
        assert!(!parsed.key_hash.contains("prior-message-key"));

        headers.append(
            IDEMPOTENCY_RECOVERY_KEY_HEADER,
            HeaderValue::from_static("duplicate"),
        );
        assert!(recovery_key(&headers, &principal).is_err());
    }

    #[test]
    fn run_fingerprint_canonicalizes_object_keys_but_not_array_order() {
        let first = serde_json::json!({"b": [1, 2], "a": true});
        let reordered = serde_json::json!({"a": true, "b": [1, 2]});
        let changed_array = serde_json::json!({"a": true, "b": [2, 1]});
        assert_eq!(
            run_fingerprint("flow", Some(&first)),
            run_fingerprint("flow", Some(&reordered))
        );
        assert_ne!(
            run_fingerprint("flow", Some(&first)),
            run_fingerprint("flow", Some(&changed_array))
        );
        assert_ne!(
            run_fingerprint("flow", None),
            run_fingerprint("flow", Some(&Value::Null))
        );
    }

    #[test]
    fn absent_and_empty_message_images_have_one_fingerprint() {
        assert_eq!(
            conversation_message_fingerprint("flow", "c1", "incarnation", "hello", None),
            conversation_message_fingerprint("flow", "c1", "incarnation", "hello", Some(&[]),)
        );
    }

    #[test]
    fn recreated_conversation_has_a_distinct_message_fingerprint() {
        assert_ne!(
            conversation_message_fingerprint("flow", "c1", "first", "hello", None),
            conversation_message_fingerprint("flow", "c1", "second", "hello", None),
        );
    }

    #[test]
    fn bounded_response_never_retains_an_oversized_body() {
        let response = serde_json::json!({"value": "x".repeat(100)});
        assert!(bounded_response_json(&response, 256).unwrap().is_some());
        assert!(bounded_response_json(&response, 8).unwrap().is_none());
    }

    #[test]
    fn conservative_deadline_does_not_grant_response_latency() {
        let started = tokio::time::Instant::now();
        let ttl = Duration::from_secs(6);
        let response_arrived = started + Duration::from_secs(4);
        let deadline = conservative_lease_deadline(started, ttl);

        assert_eq!(deadline, started + ttl);
        assert_eq!(deadline - response_arrived, Duration::from_secs(2));
        assert_ne!(deadline, response_arrived + ttl);
    }

    #[tokio::test]
    async fn expired_deadline_wins_over_an_already_ready_heartbeat_tick() {
        let deadline = tokio::time::Instant::now();
        let mut interval = tokio::time::interval(Duration::from_secs(60));

        assert_eq!(
            wait_for_heartbeat_or_expiry(&mut interval, deadline).await,
            HeartbeatWake::LeaseExpired
        );
    }

    #[tokio::test]
    async fn ready_heartbeat_tick_wins_while_the_lease_is_live() {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
        let mut interval = tokio::time::interval(Duration::from_secs(60));

        assert_eq!(
            wait_for_heartbeat_or_expiry(&mut interval, deadline).await,
            HeartbeatWake::Tick
        );
    }

    #[tokio::test]
    async fn closed_lease_channel_records_loss_but_published_loss_is_not_double_counted() {
        let before = global_lease_losses("conversation");
        let (loss_tx, mut loss) = tokio::sync::watch::channel(false);
        loss_tx.send(true).unwrap();
        wait_for_lease_loss(&mut loss).await;
        assert_eq!(global_lease_losses("conversation"), before);

        let (loss_tx, mut loss) = tokio::sync::watch::channel(false);
        drop(loss_tx);
        wait_for_lease_loss(&mut loss).await;
        assert_eq!(global_lease_losses("conversation"), before + 1);
    }

    #[test]
    fn initial_run_heartbeat_metrics_distinguish_fencing_from_store_failure() {
        assert_eq!(
            initial_run_heartbeat_metric(&Some(Ok(RunFenceHeartbeat::Owned))),
            InitialRunHeartbeatMetric::None
        );
        assert_eq!(
            initial_run_heartbeat_metric(&Some(Ok(RunFenceHeartbeat::Lost))),
            InitialRunHeartbeatMetric::LeaseLoss
        );
        assert_eq!(
            initial_run_heartbeat_metric(&Some(Err(IronCrewError::Conflict("fenced".into())))),
            InitialRunHeartbeatMetric::LeaseLoss
        );
        assert_eq!(
            initial_run_heartbeat_metric(&Some(Err(IronCrewError::Provider(
                "database unavailable".into()
            )))),
            InitialRunHeartbeatMetric::StoreFailure
        );
        assert_eq!(
            initial_run_heartbeat_metric(&None),
            InitialRunHeartbeatMetric::StoreFailure
        );
    }

    #[tokio::test]
    async fn run_fence_admission_uses_the_latest_successful_renewal() {
        let now = tokio::time::Instant::now();
        let initial_deadline = now + Duration::from_millis(5);
        let renewed_deadline = now + Duration::from_secs(60);
        let (coordinator, _outcome) = RunFenceCoordinator::new(initial_deadline);

        assert!(coordinator.renew_if_live(initial_deadline, renewed_deadline));
        tokio::time::sleep_until(initial_deadline).await;
        assert!(tokio::time::Instant::now() >= initial_deadline);

        let (start_tx, start_rx) = tokio::sync::oneshot::channel();
        assert_eq!(coordinator.admit_execution(start_tx), None);
        assert_eq!(start_rx.await, Ok(()));
    }

    #[test]
    fn failed_heartbeat_keeps_the_old_deadline_and_timely_retry_recovers() {
        let started = tokio::time::Instant::now();
        let ttl = Duration::from_secs(6);
        let initial_deadline = conservative_lease_deadline(started, ttl);

        // A failed first scheduled heartbeat does not grant more lease time.
        let failed_at = started + Duration::from_secs(2);
        let mut lease_deadline = initial_deadline;
        assert!(failed_at < lease_deadline);
        assert_eq!(lease_deadline, initial_deadline);

        // The following scheduled attempt succeeds before expiry and starts a
        // fresh conservative window at invocation, not response arrival.
        let retry_started = started + Duration::from_secs(4);
        lease_deadline = conservative_lease_deadline(retry_started, ttl);
        assert_eq!(lease_deadline, started + Duration::from_secs(10));
        assert!(initial_deadline < lease_deadline);
    }
}
