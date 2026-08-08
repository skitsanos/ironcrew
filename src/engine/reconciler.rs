//! Lease-aware stuck-run reconciliation.
//!
//! Every in-flight run carries an owner instance id and a renewable deadline.
//! Startup reconciliation abandons only expired (or legacy, unleased) rows, so
//! a rolling deployment cannot sweep healthy work owned by another pod.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use chrono::Utc;

use crate::engine::store::StateStore;
use crate::utils::error::{IronCrewError, Result};

#[derive(Debug)]
pub struct MaintenanceCycle {
    pub heartbeat: Result<usize>,
    pub reconciliation: Result<usize>,
}

impl MaintenanceCycle {
    pub fn healthy(&self) -> bool {
        self.heartbeat.is_ok() && self.reconciliation.is_ok()
    }
}

fn timeout_error(operation: &str, timeout: std::time::Duration) -> IronCrewError {
    IronCrewError::Validation(format!(
        "Run lease {operation} exceeded its {} ms maintenance timeout",
        timeout.as_millis()
    ))
}

pub async fn heartbeat_owned_runs_bounded(store: &Arc<dyn StateStore>) -> Result<usize> {
    let Some(timeout) = store.run_maintenance_watchdog() else {
        return store.heartbeat_owned_runs().await;
    };
    tokio::time::timeout(timeout, store.heartbeat_owned_runs())
        .await
        .map_err(|_| timeout_error("heartbeat", timeout))?
}

pub async fn reconcile_stuck_runs(store: &Arc<dyn StateStore>) -> Result<usize> {
    let now = Utc::now().to_rfc3339();
    reconcile_stuck_runs_at(store, &now).await
}

pub async fn reconcile_stuck_runs_at(store: &Arc<dyn StateStore>, now: &str) -> Result<usize> {
    let count = if let Some(timeout) = store.run_maintenance_watchdog() {
        tokio::time::timeout(timeout, store.reconcile_abandoned_runs(now))
            .await
            .map_err(|_| timeout_error("reconciliation", timeout))??
    } else {
        store.reconcile_abandoned_runs(now).await?
    };

    if count > 0 {
        tracing::warn!(
            "Stuck-run reconciler: marked {} expired run(s) Abandoned",
            count
        );
    } else {
        tracing::debug!("Stuck-run reconciler: no expired runs");
    }
    Ok(count)
}

/// Run one bounded periodic lease-maintenance cycle. A failed heartbeat drops
/// readiness immediately; only a complete successful heartbeat+reconcile cycle
/// can restore it.
pub async fn maintain_run_leases(
    store: &Arc<dyn StateStore>,
    maintenance_healthy: &AtomicBool,
) -> MaintenanceCycle {
    let heartbeat = heartbeat_owned_runs_bounded(store).await;
    if heartbeat.is_err() {
        maintenance_healthy.store(false, Ordering::Release);
    }

    let reconciliation = reconcile_stuck_runs(store).await;
    let cycle = MaintenanceCycle {
        heartbeat,
        reconciliation,
    };
    maintenance_healthy.store(cycle.healthy(), Ordering::Release);
    cycle
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::run_history::{JsonFileStore, RunStatus};

    #[tokio::test]
    async fn reconciler_flips_running_and_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let store: Arc<dyn StateStore> =
            Arc::new(JsonFileStore::new(dir.path().to_path_buf()).unwrap());

        // Seed one Running record via the intent API.
        store
            .save_run_intent(crate::engine::run_history::RunIntent {
                suggested_id: Some("orphan-1".into()),
                flow_name: "flow-a".into(),
                flow: "flow-a".into(),
                started_at: "2026-04-23T10:00:00Z".into(),
                agent_count: 1,
                task_count: 1,
                ..Default::default()
            })
            .await
            .unwrap();

        // First call: 1 record reconciled.
        let first = reconcile_stuck_runs_at(&store, "9999-01-01T00:00:00Z")
            .await
            .unwrap();
        assert_eq!(first, 1);

        // Record is now Abandoned.
        let r = store.get_run("orphan-1").await.unwrap();
        assert_eq!(r.status, RunStatus::Abandoned);
        assert!(!r.finished_at.is_empty());

        // Second call: 0 — nothing to reconcile anymore.
        let second = reconcile_stuck_runs_at(&store, "9999-01-01T00:00:01Z")
            .await
            .unwrap();
        assert_eq!(second, 0);
    }

    #[tokio::test]
    async fn reconciler_flips_waiting_for_input() {
        let dir = tempfile::tempdir().unwrap();
        let store: Arc<dyn StateStore> =
            Arc::new(JsonFileStore::new(dir.path().to_path_buf()).unwrap());

        // Seed a run suspended on ask_human at crash time.
        store
            .save_run_intent(crate::engine::run_history::RunIntent {
                suggested_id: Some("orphan-waiting".into()),
                flow_name: "flow-a".into(),
                flow: "flow-a".into(),
                started_at: "2026-07-07T10:00:00Z".into(),
                agent_count: 1,
                task_count: 1,
                ..Default::default()
            })
            .await
            .unwrap();
        store
            .update_run_status("orphan-waiting", RunStatus::WaitingForInput)
            .await
            .unwrap();

        // A crashed process can no more resume a waiting run than a running one.
        let count = reconcile_stuck_runs_at(&store, "9999-01-01T00:00:00Z")
            .await
            .unwrap();
        assert_eq!(count, 1);
        let r = store.get_run("orphan-waiting").await.unwrap();
        assert_eq!(r.status, RunStatus::Abandoned);
        assert!(!r.finished_at.is_empty());
    }

    #[tokio::test]
    async fn update_run_status_round_trip_and_terminal_guard() {
        let dir = tempfile::tempdir().unwrap();
        let store: Arc<dyn StateStore> =
            Arc::new(JsonFileStore::new(dir.path().to_path_buf()).unwrap());

        store
            .save_run_intent(crate::engine::run_history::RunIntent {
                suggested_id: Some("r1".into()),
                flow_name: "f".into(),
                flow: "f".into(),
                started_at: "2026-07-07T10:00:00Z".into(),
                agent_count: 1,
                task_count: 1,
                ..Default::default()
            })
            .await
            .unwrap();

        // Running -> WaitingForInput -> Running (ask_human suspend/resume).
        store
            .update_run_status("r1", RunStatus::WaitingForInput)
            .await
            .unwrap();
        assert_eq!(
            store.get_run("r1").await.unwrap().status,
            RunStatus::WaitingForInput
        );
        store
            .update_run_status("r1", RunStatus::Running)
            .await
            .unwrap();

        // A run that completed while a question was pending must still accept
        // its completion write (in-flight guard covers both statuses)...
        store
            .update_run_status("r1", RunStatus::WaitingForInput)
            .await
            .unwrap();
        store
            .update_run_completion(
                "r1",
                crate::engine::run_history::RunCompletion {
                    status: RunStatus::Failed,
                    finished_at: "2026-07-07T10:01:00Z".into(),
                    duration_ms: 60_000,
                    task_results: Vec::new(),
                    total_tokens: 0,
                    cached_tokens: 0,
                },
            )
            .await
            .unwrap();

        // ...and once terminal, status flips are rejected.
        assert!(
            store
                .update_run_status("r1", RunStatus::Running)
                .await
                .is_err()
        );
        assert!(
            store
                .update_run_status("missing", RunStatus::Running)
                .await
                .is_err()
        );
    }
}
