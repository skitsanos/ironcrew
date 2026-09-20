use super::*;

impl PostgresStore {
    pub(super) async fn heartbeat_owned_runs_maintenance(&self) -> Result<usize> {
        let mut tx = self.pool.begin().await.map_err(|error| {
            IronCrewError::Validation(format!("PG heartbeat transaction: {error}"))
        })?;
        self.configure_run_lease_transaction(&mut tx).await?;
        self.lock_run_fence(&mut tx, true).await?;
        let (_, deadline) = self
            .database_clock_with_deadline(
                &mut tx,
                self.lease.ttl().as_secs(),
                "run heartbeat lease",
            )
            .await?;
        let sql = format!(
            "UPDATE {runs} AS run SET lease_expires_at = $1
             WHERE run.owner_instance_id = $2
               AND run.status IN ('running', 'waiting_for_input')
               AND NOT EXISTS (
                   SELECT 1 FROM {idempotency} AS idem
                   WHERE idem.operation = $3 AND idem.resource_id = run.run_id
               )",
            runs = self.table_name,
            idempotency = self.idempotency_table
        );
        let result = sqlx::query(sqlx::AssertSqlSafe(sql.to_string()))
            .bind(&deadline)
            .bind(self.lease.instance_id())
            .bind(RUN_OPERATION)
            .execute(&mut *tx)
            .await
            .map_err(|e| IronCrewError::Validation(format!("PG heartbeat: {}", e)))?;
        tx.commit()
            .await
            .map_err(|error| IronCrewError::Validation(format!("PG heartbeat commit: {error}")))?;
        Ok(result.rows_affected() as usize)
    }

    pub(super) async fn health_check_maintenance(&self) -> Result<()> {
        self.verify_required_schema().await?;

        // Exercise the write privilege used by heartbeat/finalization without
        // mutating a row. A read-only credential must never report ready.
        let mut transaction = self.pool.begin().await.map_err(|e| {
            IronCrewError::Validation(format!("PostgreSQL health transaction: {e}"))
        })?;
        let sql = format!(
            "UPDATE {} SET lease_expires_at = lease_expires_at WHERE FALSE",
            self.table_name
        );
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .execute(&mut *transaction)
            .await
            .map_err(|e| {
                IronCrewError::Validation(format!("PostgreSQL health write probe: {e}"))
            })?;
        let idempotency_sql = format!(
            "UPDATE {} SET updated_at = updated_at WHERE FALSE",
            self.idempotency_table
        );
        sqlx::query(sqlx::AssertSqlSafe(idempotency_sql))
            .execute(&mut *transaction)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL idempotency health write probe: {error}"
                ))
            })?;
        let accounting_sql = format!(
            "UPDATE {} SET updated_at = updated_at WHERE FALSE",
            self.idempotency_accounting_table
        );
        sqlx::query(sqlx::AssertSqlSafe(accounting_sql))
            .execute(&mut *transaction)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL idempotency accounting health write probe: {error}"
                ))
            })?;
        let human_input_sql = format!(
            "UPDATE {} SET expires_at = expires_at WHERE FALSE",
            self.human_inputs_table
        );
        sqlx::query(sqlx::AssertSqlSafe(human_input_sql))
            .execute(&mut *transaction)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL human-input mailbox health write probe: {error}"
                ))
            })?;
        for (table, column) in [
            (&self.run_events_table, "created_at"),
            (&self.run_event_state_table, "updated_at"),
            (&self.run_event_usage_table, "updated_at"),
        ] {
            let sql = format!("UPDATE {table} SET {column} = {column} WHERE FALSE");
            sqlx::query(sqlx::AssertSqlSafe(sql))
                .execute(&mut *transaction)
                .await
                .map_err(|error| {
                    IronCrewError::Validation(format!(
                        "PostgreSQL run-event journal health write probe failed for '{table}': {error}"
                    ))
                })?;
        }
        transaction
            .rollback()
            .await
            .map_err(|e| IronCrewError::Validation(format!("PostgreSQL health rollback: {e}")))?;
        Ok(())
    }

    pub(super) async fn reconcile_abandoned_runs_maintenance(&self, now: &str) -> Result<usize> {
        parse_timestamp("reconciliation timestamp", now)?;
        let mut tx = self.pool.begin().await.map_err(|error| {
            IronCrewError::Validation(format!("PG reconcile transaction: {error}"))
        })?;
        self.configure_run_lease_transaction(&mut tx).await?;
        self.lock_idempotency_quota(&mut tx).await?;
        self.lock_run_fence(&mut tx, false).await?;
        let (database_now, _) = self
            .database_clock_with_deadline(&mut tx, 0, "run reconciliation")
            .await?;

        // A process may die after durably allocating/replying with a run id
        // but before publishing the normal run intent. Materialize those
        // tombstones and reconcile existing expired runs under one shared
        // fixed-size budget so a large history cannot repeatedly roll back
        // without making progress.
        let reserved = RUN_RECONCILIATION_BATCH_SIZE / 2;
        let mut fallback_ids = self
            .materialize_abandoned_claim_batch(&mut tx, &database_now, reserved)
            .await?;
        let mut expired_ids = self
            .reconcile_expired_run_batch(&mut tx, &database_now, reserved)
            .await?;
        let selected = i64::try_from(fallback_ids.len().saturating_add(expired_ids.len()))
            .map_err(|_| {
                IronCrewError::Validation("PostgreSQL reconciliation batch size overflow".into())
            })?;
        let remaining = RUN_RECONCILIATION_BATCH_SIZE.saturating_sub(selected);
        if remaining > 0 && fallback_ids.len() == reserved as usize {
            fallback_ids.extend(
                self.materialize_abandoned_claim_batch(&mut tx, &database_now, remaining)
                    .await?,
            );
        } else if remaining > 0 && expired_ids.len() == reserved as usize {
            expired_ids.extend(
                self.reconcile_expired_run_batch(&mut tx, &database_now, remaining)
                    .await?,
            );
        }
        let reconciled = fallback_ids.len().saturating_add(expired_ids.len());
        fallback_ids.append(&mut expired_ids);

        // Keep every dependent write within the same transaction, but scope
        // it to this batch. Independent conversation tombstones and mailbox
        // expiry/terminal repair each receive their own fixed-size budget.
        self.finalize_reconciled_runs(&mut tx, &database_now, &fallback_ids)
            .await?;
        self.reconcile_expired_conversation_batch(&mut tx, &database_now)
            .await?;
        self.delete_expired_human_input_batch(&mut tx, &database_now)
            .await?;
        tx.commit()
            .await
            .map_err(|error| IronCrewError::Validation(format!("PG reconcile commit: {error}")))?;
        // Keep journal cleanup outside the core reconciliation transaction so
        // usage-row contention or malformed journal data cannot undo critical
        // run/idempotency/HITL recovery.
        self.prune_expired_run_events_best_effort().await;
        Ok(reconciled)
    }

    pub(super) async fn begin_owner_drain_maintenance(&self) -> Result<usize> {
        let mut tx = self.pool.begin().await.map_err(|error| {
            IronCrewError::Validation(format!(
                "PostgreSQL owner-drain transaction failed: {error}"
            ))
        })?;
        self.configure_run_lease_transaction(&mut tx).await?;
        // This one-time exclusive fence serializes with run claim, intent,
        // heartbeat, cancellation, HITL, and terminalization transactions.
        self.lock_run_fence(&mut tx, false).await?;
        let (database_now, _) = self
            .database_clock_with_deadline(&mut tx, 0, "owner drain")
            .await?;
        let sql = format!(
            "UPDATE {} SET \
                 owner_draining_at = COALESCE(owner_draining_at, $1), \
                 updated_at = CASE WHEN owner_draining_at IS NULL THEN $1 ELSE updated_at END \
             WHERE operation = $2 AND owner_instance_id = $3 \
               AND state IN ('claimed', 'running')",
            self.idempotency_table
        );
        let result = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(&database_now)
            .bind(RUN_OPERATION)
            .bind(self.lease.instance_id())
            .execute(&mut *tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL owner-drain fence update failed: {error}"
                ))
            })?;
        tx.commit().await.map_err(|error| {
            IronCrewError::Validation(format!(
                "PostgreSQL owner-drain fence commit failed: {error}"
            ))
        })?;
        usize::try_from(result.rows_affected()).map_err(|_| {
            IronCrewError::Validation(
                "PostgreSQL owner-drain fence count exceeded process limits".into(),
            )
        })
    }
}
