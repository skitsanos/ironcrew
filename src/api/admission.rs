//! Principal-aware process admission and low-cardinality operational metrics.
//!
//! The supported Railway/OpenShift topology has one HTTP executor. Durable
//! idempotency fencing coordinates storage, while these bounded token buckets
//! shed abusive request bursts before expensive flow/provider work begins.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use axum::extract::{Extension, Request, State};
use axum::http::{Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use super::auth::Principal;
use crate::engine::idempotency::{IdempotencyLimits, IdempotencyUsage, PrincipalId};
use crate::engine::store::StateStore;
use crate::utils::error::{IronCrewError, Result};

const TOKEN_UNITS: u128 = 1_000_000_000;
const NANOS_PER_SECOND: u128 = 1_000_000_000;
const SECONDS_PER_MINUTE: u128 = 60;
const DEFAULT_WORK_RATE_PER_MINUTE: u64 = 60;
const DEFAULT_WORK_BURST: u64 = 10;
const DEFAULT_CONTROL_RATE_PER_MINUTE: u64 = 120;
const DEFAULT_CONTROL_BURST: u64 = 20;
const DEFAULT_OBSERVATION_RATE_PER_MINUTE: u64 = 600;
const DEFAULT_OBSERVATION_BURST: u64 = 20;
const HARD_MAX_RATE_PER_MINUTE: u64 = 60_000;
const HARD_MAX_BURST: u64 = 10_000;
const HARD_MAX_OBSERVATION_BURST: u64 = 1_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MutationClass {
    Work,
    Control,
    /// Bounded read-side polling that must not consume mutation capacity.
    Observation,
}

impl MutationClass {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Work => "work",
            Self::Control => "control",
            Self::Observation => "observation",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct RatePolicy {
    pub rate_per_minute: u64,
    pub burst: u64,
}

#[derive(Debug, Clone)]
pub struct AdmissionConfig {
    pub work: RatePolicy,
    pub control: RatePolicy,
}

impl AdmissionConfig {
    pub fn from_env() -> Result<Self> {
        Ok(Self {
            work: RatePolicy {
                rate_per_minute: bounded_env_u64(
                    "IRONCREW_ADMISSION_WORK_RATE_PER_MINUTE",
                    DEFAULT_WORK_RATE_PER_MINUTE,
                    1,
                    HARD_MAX_RATE_PER_MINUTE,
                )?,
                burst: bounded_env_u64(
                    "IRONCREW_ADMISSION_WORK_BURST",
                    DEFAULT_WORK_BURST,
                    1,
                    HARD_MAX_BURST,
                )?,
            },
            control: RatePolicy {
                rate_per_minute: bounded_env_u64(
                    "IRONCREW_ADMISSION_CONTROL_RATE_PER_MINUTE",
                    DEFAULT_CONTROL_RATE_PER_MINUTE,
                    1,
                    HARD_MAX_RATE_PER_MINUTE,
                )?,
                burst: bounded_env_u64(
                    "IRONCREW_ADMISSION_CONTROL_BURST",
                    DEFAULT_CONTROL_BURST,
                    1,
                    HARD_MAX_BURST,
                )?,
            },
        })
    }
}

impl Default for AdmissionConfig {
    fn default() -> Self {
        Self {
            work: RatePolicy {
                rate_per_minute: DEFAULT_WORK_RATE_PER_MINUTE,
                burst: DEFAULT_WORK_BURST,
            },
            control: RatePolicy {
                rate_per_minute: DEFAULT_CONTROL_RATE_PER_MINUTE,
                burst: DEFAULT_CONTROL_BURST,
            },
        }
    }
}

#[derive(Debug)]
struct TokenBucket {
    tokens: u128,
    updated_at: Instant,
}

impl TokenBucket {
    fn full(policy: RatePolicy, now: Instant) -> Self {
        Self {
            tokens: u128::from(policy.burst).saturating_mul(TOKEN_UNITS),
            updated_at: now,
        }
    }

    fn try_take(&mut self, policy: RatePolicy, now: Instant) -> std::result::Result<(), u64> {
        let elapsed_nanos = now
            .checked_duration_since(self.updated_at)
            .unwrap_or(Duration::ZERO)
            .as_nanos();
        // At TOKEN_UNITS=1e9, elapsed nanoseconds * tokens/minute / 60
        // produces token units directly without floating-point drift.
        let replenished =
            elapsed_nanos.saturating_mul(u128::from(policy.rate_per_minute)) / SECONDS_PER_MINUTE;
        let capacity = u128::from(policy.burst).saturating_mul(TOKEN_UNITS);
        self.tokens = self.tokens.saturating_add(replenished).min(capacity);
        self.updated_at = now;

        if self.tokens >= TOKEN_UNITS {
            self.tokens -= TOKEN_UNITS;
            return Ok(());
        }

        let deficit = TOKEN_UNITS - self.tokens;
        let retry_nanos = deficit
            .saturating_mul(SECONDS_PER_MINUTE)
            .div_ceil(u128::from(policy.rate_per_minute));
        let retry_seconds = retry_nanos.div_ceil(NANOS_PER_SECOND).max(1);
        Err(u64::try_from(retry_seconds).unwrap_or(u64::MAX))
    }
}

#[derive(Debug, Clone, Copy)]
pub enum QuotaMetric {
    GlobalRecords = 0,
    PrincipalRecords = 1,
    PrincipalInFlight = 2,
}

#[derive(Default)]
pub struct AdmissionMetrics {
    work_admitted: AtomicU64,
    work_limited: AtomicU64,
    control_admitted: AtomicU64,
    control_limited: AtomicU64,
    observation_admitted: AtomicU64,
    observation_limited: AtomicU64,
    internal_errors: AtomicU64,
    quota_rejections: [AtomicU64; 3],
}

impl AdmissionMetrics {
    fn record_admitted(&self, class: MutationClass) {
        match class {
            MutationClass::Work => self.work_admitted.fetch_add(1, Ordering::Relaxed),
            MutationClass::Control => self.control_admitted.fetch_add(1, Ordering::Relaxed),
            MutationClass::Observation => self.observation_admitted.fetch_add(1, Ordering::Relaxed),
        };
    }

    fn record_limited(&self, class: MutationClass) {
        match class {
            MutationClass::Work => self.work_limited.fetch_add(1, Ordering::Relaxed),
            MutationClass::Control => self.control_limited.fetch_add(1, Ordering::Relaxed),
            MutationClass::Observation => self.observation_limited.fetch_add(1, Ordering::Relaxed),
        };
    }

    fn record_internal_error(&self) {
        self.internal_errors.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_quota_rejection(&self, metric: QuotaMetric) {
        self.quota_rejections[metric as usize].fetch_add(1, Ordering::Relaxed);
    }
}

pub(super) struct AdmissionPrometheusSnapshot {
    pub(super) tracked_buckets: usize,
    pub(super) work: RatePolicy,
    pub(super) control: RatePolicy,
    pub(super) observation: RatePolicy,
    pub(super) work_admitted: u64,
    pub(super) work_limited: u64,
    pub(super) control_admitted: u64,
    pub(super) control_limited: u64,
    pub(super) observation_admitted: u64,
    pub(super) observation_limited: u64,
    pub(super) internal_errors: u64,
    pub(super) quota_rejections: [u64; 3],
}

pub struct AdmissionController {
    config: AdmissionConfig,
    observation: RatePolicy,
    buckets: Mutex<HashMap<(PrincipalId, MutationClass), TokenBucket>>,
    metrics: AdmissionMetrics,
    durable_usage: tokio::sync::Mutex<Option<(Instant, IdempotencyUsage)>>,
}

impl AdmissionController {
    pub fn new(config: AdmissionConfig) -> Self {
        Self::new_with_observation(
            config,
            RatePolicy {
                rate_per_minute: DEFAULT_OBSERVATION_RATE_PER_MINUTE,
                burst: DEFAULT_OBSERVATION_BURST,
            },
        )
    }

    /// Construct an admission controller with an explicit observation policy.
    ///
    /// This separate constructor keeps existing `AdmissionConfig` struct
    /// literals source-compatible while allowing embedders to tune bounded
    /// read-side polling independently from mutation traffic.
    pub fn new_with_observation(config: AdmissionConfig, observation: RatePolicy) -> Self {
        Self {
            config,
            observation,
            buckets: Mutex::new(HashMap::new()),
            metrics: AdmissionMetrics::default(),
            durable_usage: tokio::sync::Mutex::new(None),
        }
    }

    pub fn from_env() -> Result<Self> {
        let config = AdmissionConfig::from_env()?;
        let observation = RatePolicy {
            rate_per_minute: bounded_env_u64(
                "IRONCREW_ADMISSION_OBSERVATION_RATE_PER_MINUTE",
                DEFAULT_OBSERVATION_RATE_PER_MINUTE,
                1,
                HARD_MAX_RATE_PER_MINUTE,
            )?,
            burst: bounded_env_u64(
                "IRONCREW_ADMISSION_OBSERVATION_BURST",
                DEFAULT_OBSERVATION_BURST,
                1,
                HARD_MAX_OBSERVATION_BURST,
            )?,
        };
        Ok(Self::new_with_observation(config, observation))
    }

    pub fn metrics(&self) -> &AdmissionMetrics {
        &self.metrics
    }

    pub(super) fn prometheus_snapshot(&self) -> AdmissionPrometheusSnapshot {
        AdmissionPrometheusSnapshot {
            tracked_buckets: self.tracked_buckets(),
            work: self.config.work,
            control: self.config.control,
            observation: self.observation,
            work_admitted: self.metrics.work_admitted.load(Ordering::Relaxed),
            work_limited: self.metrics.work_limited.load(Ordering::Relaxed),
            control_admitted: self.metrics.control_admitted.load(Ordering::Relaxed),
            control_limited: self.metrics.control_limited.load(Ordering::Relaxed),
            observation_admitted: self.metrics.observation_admitted.load(Ordering::Relaxed),
            observation_limited: self.metrics.observation_limited.load(Ordering::Relaxed),
            internal_errors: self.metrics.internal_errors.load(Ordering::Relaxed),
            quota_rejections: std::array::from_fn(|index| {
                self.metrics.quota_rejections[index].load(Ordering::Relaxed)
            }),
        }
    }

    pub(super) async fn cached_idempotency_usage(
        &self,
        store: &dyn StateStore,
        principal: &PrincipalId,
        limits: IdempotencyLimits,
    ) -> Result<IdempotencyUsage> {
        let mut cache = self.durable_usage.lock().await;
        if let Some((checked_at, snapshot)) = *cache
            && checked_at.elapsed() < Duration::from_secs(1)
        {
            return Ok(snapshot);
        }

        match store.idempotency_usage(principal, limits).await {
            Ok(snapshot) => {
                *cache = Some((Instant::now(), snapshot));
                Ok(snapshot)
            }
            Err(error) => {
                self.metrics.record_internal_error();
                Err(error)
            }
        }
    }

    fn admit(
        &self,
        principal: &Principal,
        class: MutationClass,
    ) -> std::result::Result<(), AdmissionFailure> {
        self.admit_principal(principal.id(), class)
    }

    fn admit_principal(
        &self,
        principal: &PrincipalId,
        class: MutationClass,
    ) -> std::result::Result<(), AdmissionFailure> {
        let policy = match class {
            MutationClass::Work => self.config.work,
            MutationClass::Control => self.config.control,
            MutationClass::Observation => self.observation,
        };
        let now = Instant::now();
        let mut buckets = self.buckets.lock().map_err(|_| {
            self.metrics.record_internal_error();
            AdmissionFailure::Unavailable
        })?;
        let bucket = buckets
            .entry((principal.clone(), class))
            .or_insert_with(|| TokenBucket::full(policy, now));
        match bucket.try_take(policy, now) {
            Ok(()) => {
                self.metrics.record_admitted(class);
                Ok(())
            }
            Err(retry_after_seconds) => {
                self.metrics.record_limited(class);
                Err(AdmissionFailure::Limited {
                    class,
                    retry_after_seconds,
                })
            }
        }
    }

    fn tracked_buckets(&self) -> usize {
        self.buckets
            .lock()
            .map(|buckets| buckets.len())
            .unwrap_or(0)
    }
}

impl Default for AdmissionController {
    fn default() -> Self {
        Self::new(AdmissionConfig::default())
    }
}

enum AdmissionFailure {
    Limited {
        class: MutationClass,
        retry_after_seconds: u64,
    },
    Unavailable,
}

pub async fn enforce_mutation_admission(
    State(controller): State<std::sync::Arc<AdmissionController>>,
    Extension(principal): Extension<Principal>,
    request: Request,
    next: Next,
) -> Response {
    let Some(class) = classify_mutation(request.method(), request.uri().path()) else {
        return harden_downstream_rate_response(next.run(request).await);
    };
    match controller.admit(&principal, class) {
        Ok(()) => harden_downstream_rate_response(next.run(request).await),
        Err(AdmissionFailure::Limited {
            class,
            retry_after_seconds,
        }) => rate_limited_response(class, retry_after_seconds),
        Err(AdmissionFailure::Unavailable) => (
            StatusCode::SERVICE_UNAVAILABLE,
            [(header::CACHE_CONTROL, "no-store")],
            axum::Json(serde_json::json!({
                "error": "Request admission is temporarily unavailable"
            })),
        )
            .into_response(),
    }
}

fn harden_downstream_rate_response(mut response: Response) -> Response {
    if response.status() == StatusCode::TOO_MANY_REQUESTS {
        if !response.headers().contains_key(header::RETRY_AFTER) {
            // Durable quotas include a more precise advisory value in their
            // generic error body. Keep a conservative header fallback for
            // clients and intermediaries that only honor Retry-After.
            response
                .headers_mut()
                .insert(header::RETRY_AFTER, "60".parse().unwrap());
        }
        response
            .headers_mut()
            .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    }
    response
}

pub(crate) fn classify_mutation(method: &Method, path: &str) -> Option<MutationClass> {
    // Polling a shared question mailbox performs indexed database reads plus
    // bounded ciphertext decryption. Give it an independently bounded bucket
    // so an aggressive UI poll loop cannot starve answer/abort capacity.
    if method == Method::GET && is_question_poll_path(path) {
        return Some(MutationClass::Observation);
    }
    if method == Method::DELETE {
        return Some(MutationClass::Control);
    }
    if method != Method::POST {
        return None;
    }
    if is_run_control_path(path) {
        Some(MutationClass::Control)
    } else {
        Some(MutationClass::Work)
    }
}

fn is_run_control_path(path: &str) -> bool {
    let mut segments = path.trim_matches('/').split('/');
    matches!(
        (
            segments.next(),
            segments.next(),
            segments.next(),
            segments.next(),
            segments.next(),
        ),
        (Some("flows"), Some(flow), Some("abort" | "answer"), Some(run_id), None)
            if !flow.is_empty() && !run_id.is_empty()
    )
}

fn is_question_poll_path(path: &str) -> bool {
    let mut segments = path.trim_matches('/').split('/');
    matches!(
        (
            segments.next(),
            segments.next(),
            segments.next(),
            segments.next(),
            segments.next(),
        ),
        (Some("flows"), Some(flow), Some("questions"), Some(run_id), None)
            if !flow.is_empty() && !run_id.is_empty()
    )
}

fn rate_limited_response(class: MutationClass, retry_after_seconds: u64) -> Response {
    let mut response = (
        StatusCode::TOO_MANY_REQUESTS,
        axum::Json(serde_json::json!({
            "error": format!(
                "{} request rate limit exceeded; retry later",
                class.label()
            )
        })),
    )
        .into_response();
    let retry_after = retry_after_seconds.max(1).to_string();
    response.headers_mut().insert(
        header::RETRY_AFTER,
        retry_after
            .parse()
            .expect("numeric Retry-After must be a valid header value"),
    );
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    response
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_bucket_refills_without_floating_point_drift() {
        let policy = RatePolicy {
            rate_per_minute: 2,
            burst: 2,
        };
        let start = Instant::now();
        let mut bucket = TokenBucket::full(policy, start);
        assert_eq!(bucket.try_take(policy, start), Ok(()));
        assert_eq!(bucket.try_take(policy, start), Ok(()));
        assert_eq!(bucket.try_take(policy, start), Err(30));
        assert_eq!(
            bucket.try_take(policy, start + Duration::from_secs(30)),
            Ok(())
        );
    }

    #[test]
    fn retry_after_rounds_up_and_never_returns_zero() {
        let policy = RatePolicy {
            rate_per_minute: 60,
            burst: 1,
        };
        let start = Instant::now();
        let mut bucket = TokenBucket::full(policy, start);
        assert_eq!(bucket.try_take(policy, start), Ok(()));
        assert_eq!(
            bucket.try_take(policy, start + Duration::from_millis(1)),
            Err(1)
        );
    }

    #[test]
    fn work_control_and_observation_routes_are_separate() {
        assert_eq!(
            classify_mutation(&Method::POST, "/flows/a/run"),
            Some(MutationClass::Work)
        );
        assert_eq!(
            classify_mutation(&Method::POST, "/flows/a/conversations/c/messages"),
            Some(MutationClass::Work)
        );
        assert_eq!(
            classify_mutation(&Method::POST, "/flows/a/abort/r"),
            Some(MutationClass::Control)
        );
        assert_eq!(
            classify_mutation(&Method::DELETE, "/flows/a/runs/r"),
            Some(MutationClass::Control)
        );
        assert_eq!(
            classify_mutation(&Method::GET, "/flows/a/questions/run-1"),
            Some(MutationClass::Observation)
        );
        assert_eq!(
            classify_mutation(&Method::GET, "/other/a/questions/run-1"),
            None
        );
        assert_eq!(
            classify_mutation(&Method::GET, "/flows/a/questions/run-1/extra"),
            None
        );
        assert_eq!(classify_mutation(&Method::GET, "/metrics"), None);

        for path in [
            "/flows/abort/run",
            "/flows/answer/run",
            "/flows/a/conversations/abort/start",
            "/flows/a/conversations/answer/messages",
            "/flows/a/abort/run-id/extra",
            "/flows/a/answer/run-id/extra",
        ] {
            assert_eq!(
                classify_mutation(&Method::POST, path),
                Some(MutationClass::Work),
                "attacker-controlled path segment must not select control admission: {path}"
            );
        }

        for path in ["/flows/a/abort/run-id", "/flows/a/answer/run-id"] {
            assert_eq!(
                classify_mutation(&Method::POST, path),
                Some(MutationClass::Control),
                "exact run-control route must retain control admission: {path}"
            );
        }
    }

    fn test_controller(observation: RatePolicy, control: RatePolicy) -> AdmissionController {
        AdmissionController::new_with_observation(
            AdmissionConfig {
                work: RatePolicy {
                    rate_per_minute: 1,
                    burst: 1,
                },
                control,
            },
            observation,
        )
    }

    #[test]
    fn sustained_question_polling_cannot_deplete_control_capacity() {
        let controller = test_controller(
            RatePolicy {
                rate_per_minute: 1,
                burst: 2,
            },
            RatePolicy {
                rate_per_minute: 1,
                burst: 1,
            },
        );
        let principal = PrincipalId::anonymous();

        assert!(
            controller
                .admit_principal(&principal, MutationClass::Observation)
                .is_ok()
        );
        assert!(
            controller
                .admit_principal(&principal, MutationClass::Observation)
                .is_ok()
        );
        assert!(matches!(
            controller.admit_principal(&principal, MutationClass::Observation),
            Err(AdmissionFailure::Limited {
                class: MutationClass::Observation,
                ..
            })
        ));

        // The answer/abort/delete class retains its full independent burst.
        assert!(
            controller
                .admit_principal(&principal, MutationClass::Control)
                .is_ok()
        );
        assert!(matches!(
            controller.admit_principal(&principal, MutationClass::Control),
            Err(AdmissionFailure::Limited {
                class: MutationClass::Control,
                ..
            })
        ));
        assert_eq!(controller.tracked_buckets(), 2);
    }

    #[test]
    fn question_polling_has_an_independent_bounded_burst() {
        let controller = test_controller(
            RatePolicy {
                rate_per_minute: 1,
                burst: 1,
            },
            RatePolicy {
                rate_per_minute: 1,
                burst: 5,
            },
        );
        let principal = PrincipalId::anonymous();

        assert!(
            controller
                .admit_principal(&principal, MutationClass::Observation)
                .is_ok()
        );
        assert!(matches!(
            controller.admit_principal(&principal, MutationClass::Observation),
            Err(AdmissionFailure::Limited {
                class: MutationClass::Observation,
                retry_after_seconds: 60,
            })
        ));
        assert_eq!(
            controller
                .metrics()
                .observation_admitted
                .load(Ordering::Relaxed),
            1
        );
        assert_eq!(
            controller
                .metrics()
                .observation_limited
                .load(Ordering::Relaxed),
            1
        );
    }

    #[test]
    fn rate_limit_response_is_non_cacheable_and_retryable() {
        let response = rate_limited_response(MutationClass::Work, 0);
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(response.headers()[header::RETRY_AFTER], "1");
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    }
}
