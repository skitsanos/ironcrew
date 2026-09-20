use super::*;

mod audit;
mod events;
mod human_input;
mod idempotency;
mod idempotency_accounting;
mod runs;
mod sessions;
mod verify_accounting;
mod verify_bootstrap;
mod verify_core;
mod verify_events;
mod verify_human_input;
mod verify_idempotency;

impl PostgresStore {
    /// Bootstrap the database atomically, then verify every readiness invariant.
    pub(super) async fn bootstrap(&self) -> Result<()> {
        let mut tx = self.pool.begin().await.map_err(|error| {
            IronCrewError::Validation(format!("Failed to begin PostgreSQL bootstrap: {error}"))
        })?;
        self.lock_advisory(&mut tx, "bootstrap", "global", false)
            .await?;
        self.bootstrap_runs(&mut tx).await?;
        self.bootstrap_sessions(&mut tx).await?;
        self.bootstrap_audit(&mut tx).await?;
        self.bootstrap_idempotency(&mut tx).await?;
        self.bootstrap_idempotency_accounting(&mut tx).await?;
        self.bootstrap_human_input(&mut tx).await?;
        self.bootstrap_run_events(&mut tx).await?;
        self.verify_bootstrap_run_columns(&mut tx).await?;
        tx.commit().await.map_err(|error| {
            IronCrewError::Validation(format!("Failed to commit PostgreSQL bootstrap: {error}"))
        })?;
        self.verify_required_schema().await?;
        tracing::debug!(
            "PostgreSQL bootstrap complete for tables '{}', '{}', '{}', '{}', '{}', '{}', '{}', '{}', '{}', '{}'",
            self.table_name,
            self.conversations_table,
            self.dialogs_table,
            self.audit_events_table,
            self.idempotency_table,
            self.idempotency_accounting_table,
            self.human_inputs_table,
            self.run_events_table,
            self.run_event_state_table,
            self.run_event_usage_table
        );
        Ok(())
    }

    /// Verify invariants required for safe multi-instance operation. Readiness
    /// uses the same check, so a manually altered schema cannot remain ready.
    pub(super) async fn verify_required_schema(&self) -> Result<()> {
        self.verify_core_schema().await?;
        self.verify_idempotency_schema().await?;
        self.verify_human_input_schema().await?;
        self.verify_idempotency_accounting_schema().await?;
        self.verify_run_event_schema().await?;
        Ok(())
    }
}
