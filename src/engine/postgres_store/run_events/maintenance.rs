use super::super::PostgresStore;

impl PostgresStore {
    /// Opportunistic physical retention sweep run after core reconciliation
    /// commits. Failure or lock contention is deliberately non-fatal: logical
    /// reads already filter expired rows, and maintenance must never roll back
    /// abandonment/idempotency recovery.
    pub(in crate::engine::postgres_store) async fn prune_expired_run_events_best_effort(&self) {
        let mut tx = match self.pool.begin().await {
            Ok(tx) => tx,
            Err(error) => {
                tracing::warn!(%error, "PostgreSQL run-event maintenance transaction unavailable");
                return;
            }
        };
        if let Err(error) = self.configure_run_lease_transaction(&mut tx).await {
            let _ = tx.rollback().await;
            tracing::warn!(%error, "PostgreSQL run-event maintenance timeout setup failed");
            return;
        }
        let lock_name = format!("ironcrew:{}:run-event-maintenance", self.run_events_table);
        let acquired: bool =
            match sqlx::query_scalar("SELECT pg_try_advisory_xact_lock(hashtextextended($1, 0))")
                .bind(lock_name)
                .fetch_one(&mut *tx)
                .await
            {
                Ok(acquired) => acquired,
                Err(error) => {
                    let _ = tx.rollback().await;
                    tracing::warn!(%error, "PostgreSQL run-event maintenance lock probe failed");
                    return;
                }
            };
        if !acquired {
            let _ = tx.rollback().await;
            return;
        }
        if let Err(error) = sqlx::query("SELECT set_config('lock_timeout', '100ms', true)")
            .execute(&mut *tx)
            .await
        {
            let _ = tx.rollback().await;
            tracing::warn!(%error, "PostgreSQL run-event maintenance timeout setup failed");
            return;
        }
        if let Err(error) = self.lock_run_event_usage(&mut tx).await {
            let _ = tx.rollback().await;
            tracing::debug!(%error, "PostgreSQL run-event maintenance skipped on usage contention");
            return;
        }
        if let Err(error) = self.prune_expired_run_events(&mut tx).await {
            let _ = tx.rollback().await;
            tracing::warn!(%error, "PostgreSQL run-event maintenance prune failed");
            return;
        }
        if let Err(error) = tx.commit().await {
            tracing::warn!(%error, "PostgreSQL run-event maintenance commit failed");
        }
    }
}
