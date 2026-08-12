use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};

use axum::{
    extract::{Request, State},
    http::{HeaderName, HeaderValue, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};

use super::{AppState, admission::MutationClass, auth::Principal};

pub const INSTANCE_ID_HEADER: HeaderName = HeaderName::from_static("x-ironcrew-instance-id");

/// Monotonic process lifecycle. A process can only become accepting again by
/// restarting, which prevents a failed durable fence from reopening traffic.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum LifecyclePhase {
    Accepting = 0,
    Fencing = 1,
    Draining = 2,
    Stopping = 3,
}

impl LifecyclePhase {
    pub const ALL: [Self; 4] = [
        Self::Accepting,
        Self::Fencing,
        Self::Draining,
        Self::Stopping,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Accepting => "accepting",
            Self::Fencing => "fencing",
            Self::Draining => "draining",
            Self::Stopping => "stopping",
        }
    }

    fn from_u8(value: u8) -> Self {
        match value {
            0 => Self::Accepting,
            1 => Self::Fencing,
            2 => Self::Draining,
            _ => Self::Stopping,
        }
    }
}

#[derive(Debug)]
pub struct LifecycleController {
    phase: AtomicU8,
    work_rejections: AtomicU64,
    control_rejections: AtomicU64,
}

impl LifecycleController {
    pub const fn new() -> Self {
        Self {
            phase: AtomicU8::new(LifecyclePhase::Accepting as u8),
            work_rejections: AtomicU64::new(0),
            control_rejections: AtomicU64::new(0),
        }
    }

    pub fn phase(&self) -> LifecyclePhase {
        LifecyclePhase::from_u8(self.phase.load(Ordering::Acquire))
    }

    pub fn is_accepting_mutations(&self) -> bool {
        self.phase() == LifecyclePhase::Accepting
    }

    pub fn begin_fencing(&self) -> LifecyclePhase {
        self.advance_to(LifecyclePhase::Fencing)
    }

    pub fn mark_draining(&self) -> LifecyclePhase {
        self.advance_to(LifecyclePhase::Draining)
    }

    pub fn mark_stopping(&self) -> LifecyclePhase {
        self.advance_to(LifecyclePhase::Stopping)
    }

    pub fn rejection_count(&self, class: MutationClass) -> u64 {
        match class {
            MutationClass::Work => self.work_rejections.load(Ordering::Relaxed),
            MutationClass::Control => self.control_rejections.load(Ordering::Relaxed),
            MutationClass::Observation => 0,
        }
    }

    fn record_rejection(&self, class: MutationClass) {
        match class {
            MutationClass::Work => {
                self.work_rejections.fetch_add(1, Ordering::Relaxed);
            }
            MutationClass::Control => {
                self.control_rejections.fetch_add(1, Ordering::Relaxed);
            }
            MutationClass::Observation => {}
        }
    }

    fn advance_to(&self, target: LifecyclePhase) -> LifecyclePhase {
        let target = target as u8;
        let mut observed = self.phase.load(Ordering::Acquire);
        while observed < target {
            match self.phase.compare_exchange_weak(
                observed,
                target,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return LifecyclePhase::from_u8(target),
                Err(current) => observed = current,
            }
        }
        LifecyclePhase::from_u8(observed)
    }
}

impl Default for LifecycleController {
    fn default() -> Self {
        Self::new()
    }
}

/// Authenticated lifecycle boundary for the protected router. It runs after
/// bearer authentication and before rate admission so rejected drain traffic
/// cannot consume admission tokens. Reads, metrics, and SSE remain available
/// while a replica drains; only state-changing methods are fenced.
pub async fn enforce_mutation_lifecycle(
    State(state): State<std::sync::Arc<AppState>>,
    request: Request,
    next: Next,
) -> Response {
    // This snapshot is the lifecycle-admission linearization point. A request
    // admitted while Accepting remains a pre-fence request even if a deeper
    // race check later returns 503; never rewrite that downstream failure as
    // `instance_draining`, because it may describe capacity or partial work.
    let phase = state.lifecycle.phase();
    let mutation_class =
        super::admission::classify_mutation(request.method(), request.uri().path())
            .filter(|class| *class != MutationClass::Observation);
    let mut response = if let Some(class) = mutation_class
        && phase != LifecyclePhase::Accepting
    {
        state.lifecycle.record_rejection(class);
        draining_response(&state, phase)
    } else {
        next.run(request).await
    };

    if response.status() == StatusCode::SERVICE_UNAVAILABLE {
        response
            .headers_mut()
            .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        response
            .headers_mut()
            .entry(header::RETRY_AFTER)
            .or_insert(HeaderValue::from_static("1"));
    }
    response
}

/// Add process attribution only after authentication has established a
/// trusted principal. This wraps the lifecycle boundary, so both successful
/// and drain-rejected protected responses carry the selected instance id.
pub async fn attach_instance_id(
    State(state): State<std::sync::Arc<AppState>>,
    request: Request,
    next: Next,
) -> Response {
    let authenticated = request
        .extensions()
        .get::<Principal>()
        .is_some_and(Principal::is_authenticated);
    let mut response = next.run(request).await;
    if authenticated {
        let instance_id = HeaderValue::from_str(state.store.instance_id())
            .expect("validated instance id must be a valid HTTP header value");
        response
            .headers_mut()
            .insert(INSTANCE_ID_HEADER, instance_id);
    }
    response
}

fn draining_response(state: &AppState, phase: LifecyclePhase) -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        [
            (header::CACHE_CONTROL, "no-store"),
            (header::RETRY_AFTER, "1"),
        ],
        axum::Json(serde_json::json!({
            "error": "This IronCrew instance is draining and cannot accept mutations",
            "code": "instance_draining",
            "lifecycle_state": phase.as_str(),
            "instance_id": state.store.instance_id(),
        })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Method;

    #[test]
    fn lifecycle_is_monotonic_and_idempotent() {
        let lifecycle = LifecycleController::new();
        assert_eq!(lifecycle.phase(), LifecyclePhase::Accepting);
        assert_eq!(lifecycle.mark_draining(), LifecyclePhase::Draining);
        assert_eq!(lifecycle.begin_fencing(), LifecyclePhase::Draining);
        assert_eq!(lifecycle.mark_draining(), LifecyclePhase::Draining);
        assert_eq!(lifecycle.mark_stopping(), LifecyclePhase::Stopping);
        assert_eq!(lifecycle.begin_fencing(), LifecyclePhase::Stopping);
    }

    #[test]
    fn only_post_and_delete_are_mutations() {
        assert!(
            super::super::admission::classify_mutation(&Method::POST, "/flows/a/run").is_some()
        );
        assert!(
            super::super::admission::classify_mutation(&Method::DELETE, "/flows/a/runs/r")
                .is_some()
        );
        assert!(super::super::admission::classify_mutation(&Method::GET, "/metrics").is_none());
        assert!(super::super::admission::classify_mutation(&Method::HEAD, "/metrics").is_none());
        assert!(
            super::super::admission::classify_mutation(&Method::OPTIONS, "/flows/a/run").is_none()
        );
    }
}
