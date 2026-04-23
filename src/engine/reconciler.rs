//! Stuck-run reconciliation on process startup.
//!
//! Single-instance assumption: any RunRecord still marked `Running`
//! when this function runs belongs to a prior process that crashed
//! mid-run. Flips them all to `Abandoned` and logs a summary.
//!
//! Called from both `ironcrew run` (CLI) and `ironcrew serve` startup.
//! In both cases it runs BEFORE the current invocation's own
//! `save_run_intent`, so it can never sweep the in-flight record.

use std::sync::Arc;

use chrono::Utc;

use crate::engine::store::StateStore;
use crate::utils::error::Result;

pub async fn reconcile_stuck_runs(store: &Arc<dyn StateStore>) -> Result<usize> {
    let now = Utc::now().to_rfc3339();
    let count = store.reconcile_abandoned_runs(&now).await?;

    if count > 0 {
        tracing::warn!(
            "Stuck-run reconciler: flipped {} Running → Abandoned (prior process crashed)",
            count
        );
    } else {
        tracing::debug!("Stuck-run reconciler: no orphaned runs");
    }
    Ok(count)
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
            .save_run_intent(
                Some("orphan-1".into()),
                "flow-a",
                "2026-04-23T10:00:00Z",
                1,
                1,
                &[],
            )
            .await
            .unwrap();

        // First call: 1 record reconciled.
        let first = reconcile_stuck_runs(&store).await.unwrap();
        assert_eq!(first, 1);

        // Record is now Abandoned.
        let r = store.get_run("orphan-1").await.unwrap();
        assert_eq!(r.status, RunStatus::Abandoned);
        assert!(!r.finished_at.is_empty());

        // Second call: 0 — nothing to reconcile anymore.
        let second = reconcile_stuck_runs(&store).await.unwrap();
        assert_eq!(second, 0);
    }
}
