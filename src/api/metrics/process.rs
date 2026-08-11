//! Process, lifecycle, authentication, and admission series.

use std::fmt::Write as _;
use std::sync::atomic::Ordering;

use super::{write_counter, write_gauge, write_helped_gauge};
use crate::api::AppState;
use crate::api::admission::{MutationClass, QuotaMetric};

pub(super) struct Snapshot {
    run_registry_entries: usize,
    conversation_registry_entries: usize,
    active_runs: usize,
    active_conversations: usize,
    active_sse: usize,
}

impl Snapshot {
    pub(super) async fn capture(state: &AppState) -> Self {
        Self {
            run_registry_entries: state.active_runs.read().await.len(),
            conversation_registry_entries: state.active_conversations.read().await.len(),
            active_runs: state
                .max_active_runs
                .saturating_sub(state.run_permits.available_permits()),
            active_conversations: state
                .max_active_conversations
                .saturating_sub(state.conversation_permits.available_permits()),
            active_sse: state
                .max_sse_connections
                .saturating_sub(state.sse_permits.available_permits()),
        }
    }
}

pub(super) async fn append(body: &mut String, state: &AppState, snapshot: Snapshot) {
    writeln!(body, "# TYPE ironcrew_build_info gauge").unwrap();
    writeln!(
        body,
        "ironcrew_build_info{{version=\"{}\"}} 1",
        env!("CARGO_PKG_VERSION")
    )
    .unwrap();
    write_gauge(body, "ironcrew_process_active_runs", snapshot.active_runs);
    write_gauge(
        body,
        "ironcrew_process_active_runs_limit",
        state.max_active_runs,
    );
    write_gauge(
        body,
        "ironcrew_process_run_registry_entries",
        snapshot.run_registry_entries,
    );
    write_gauge(
        body,
        "ironcrew_process_active_conversations",
        snapshot.active_conversations,
    );
    write_gauge(
        body,
        "ironcrew_process_active_conversations_limit",
        state.max_active_conversations,
    );
    write_gauge(
        body,
        "ironcrew_process_conversation_registry_entries",
        snapshot.conversation_registry_entries,
    );
    write_gauge(
        body,
        "ironcrew_process_active_sse_connections",
        snapshot.active_sse,
    );
    write_gauge(
        body,
        "ironcrew_process_active_sse_connections_limit",
        state.max_sse_connections,
    );
    super::super::resource_metrics::append(body, state).await;
    write_helped_gauge(
        body,
        "ironcrew_store_maintenance_healthy",
        "Whether the latest completed store maintenance cycle succeeded (1 healthy, 0 unhealthy).",
        u8::from(state.store_maintenance_healthy.load(Ordering::Acquire)),
    );
    write_helped_gauge(
        body,
        "ironcrew_process_terminal_persistence_degraded_finalizers",
        "Current run or conversation finalizers retrying durable persistence in this process.",
        state.terminal_persistence_failures.load(Ordering::Acquire),
    );
    writeln!(
        body,
        "# HELP ironcrew_process_lifecycle_state Current process lifecycle as a fixed one-hot gauge."
    )
    .unwrap();
    writeln!(body, "# TYPE ironcrew_process_lifecycle_state gauge").unwrap();
    let lifecycle_phase = state.lifecycle.phase();
    for phase in super::super::lifecycle::LifecyclePhase::ALL {
        writeln!(
            body,
            "ironcrew_process_lifecycle_state{{state=\"{}\"}} {}",
            phase.as_str(),
            u8::from(phase == lifecycle_phase),
        )
        .unwrap();
    }
    writeln!(
        body,
        "# HELP ironcrew_process_lifecycle_rejections_total Mutation requests rejected by the process lifecycle boundary."
    )
    .unwrap();
    writeln!(
        body,
        "# TYPE ironcrew_process_lifecycle_rejections_total counter"
    )
    .unwrap();
    for class in [MutationClass::Work, MutationClass::Control] {
        writeln!(
            body,
            "ironcrew_process_lifecycle_rejections_total{{class=\"{}\"}} {}",
            class.label(),
            state.lifecycle.rejection_count(class),
        )
        .unwrap();
    }
    write_gauge(
        body,
        "ironcrew_auth_configured_principals",
        state.auth.principal_count(),
    );
    let admission = state.admission.prometheus_snapshot();
    write_gauge(
        body,
        "ironcrew_admission_tracked_buckets",
        admission.tracked_buckets,
    );
    writeln!(body, "# TYPE ironcrew_admission_rate_per_minute gauge").unwrap();
    for (class, policy) in [
        ("work", admission.work),
        ("control", admission.control),
        ("observation", admission.observation),
    ] {
        writeln!(
            body,
            "ironcrew_admission_rate_per_minute{{class=\"{class}\"}} {}",
            policy.rate_per_minute
        )
        .unwrap();
    }
    writeln!(body, "# TYPE ironcrew_admission_burst gauge").unwrap();
    for (class, policy) in [
        ("work", admission.work),
        ("control", admission.control),
        ("observation", admission.observation),
    ] {
        writeln!(
            body,
            "ironcrew_admission_burst{{class=\"{class}\"}} {}",
            policy.burst
        )
        .unwrap();
    }
    writeln!(body, "# TYPE ironcrew_admission_requests_total counter").unwrap();
    for (class, admitted, limited) in [
        ("work", admission.work_admitted, admission.work_limited),
        (
            "control",
            admission.control_admitted,
            admission.control_limited,
        ),
        (
            "observation",
            admission.observation_admitted,
            admission.observation_limited,
        ),
    ] {
        write_counter(
            body,
            &format!("ironcrew_admission_requests_total{{class=\"{class}\",outcome=\"admitted\"}}"),
            admitted,
        );
        write_counter(
            body,
            &format!("ironcrew_admission_requests_total{{class=\"{class}\",outcome=\"limited\"}}"),
            limited,
        );
    }
    writeln!(
        body,
        "# TYPE ironcrew_admission_internal_errors_total counter"
    )
    .unwrap();
    write_counter(
        body,
        "ironcrew_admission_internal_errors_total",
        admission.internal_errors,
    );
    writeln!(
        body,
        "# TYPE ironcrew_idempotency_quota_rejections_total counter"
    )
    .unwrap();
    for (metric, index) in [
        ("global_records", QuotaMetric::GlobalRecords as usize),
        ("principal_records", QuotaMetric::PrincipalRecords as usize),
        (
            "principal_in_flight",
            QuotaMetric::PrincipalInFlight as usize,
        ),
    ] {
        writeln!(
            body,
            "ironcrew_idempotency_quota_rejections_total{{resource=\"{metric}\"}} {}",
            admission.quota_rejections[index]
        )
        .unwrap();
    }
}
