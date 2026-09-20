use super::*;

impl PostgresStore {
    pub(super) async fn request_run_cancellation_maintenance(
        &self,
        run_id: &str,
        flow: &str,
    ) -> Result<RunCancellationRequest> {
        if run_id.is_empty() || run_id.len() > 128 {
            return Err(IronCrewError::Validation(
                "Cancellation run id must be 1..=128 bytes".into(),
            ));
        }
        if flow.is_empty() || flow.len() > 255 || flow.chars().any(char::is_control) {
            return Err(IronCrewError::Validation(
                "Cancellation flow must be 1..=255 printable bytes".into(),
            ));
        }

        let mut tx = self.pool.begin().await.map_err(|error| {
            IronCrewError::Validation(format!(
                "PostgreSQL run cancellation transaction failed: {error}"
            ))
        })?;
        // Match the run-intent/heartbeat lock order. This serializes the
        // cancellation request with terminalization without blocking other
        // unrelated runs.
        self.lock_run_fence(&mut tx, true).await?;
        self.lock_resource(&mut tx, RUN_OPERATION, "", run_id)
            .await?;

        let run_sql = format!(
            "SELECT status, owner_instance_id, flow FROM {} WHERE run_id = $1 FOR UPDATE",
            self.table_name
        );
        let Some(run) = sqlx::query(sqlx::AssertSqlSafe(run_sql))
            .bind(run_id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL cancellation run lookup failed: {error}"
                ))
            })?
        else {
            tx.commit().await.map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL missing cancellation run commit failed: {error}"
                ))
            })?;
            return Ok(RunCancellationRequest::NotFound);
        };
        let run_flow: String = run
            .try_get("flow")
            .map_err(|error| IronCrewError::Validation(format!("Column error: {error}")))?;
        if run_flow != flow {
            tx.commit().await.map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL scoped cancellation lookup commit failed: {error}"
                ))
            })?;
            return Ok(RunCancellationRequest::NotFound);
        }
        let status = run
            .try_get::<String, _>("status")
            .map_err(|error| IronCrewError::Validation(format!("Column error: {error}")))?
            .parse::<RunStatus>()?;
        if status.is_terminal() {
            self.delete_human_inputs_for_run(&mut tx, run_id).await?;
            tx.commit().await.map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL terminal cancellation lookup commit failed: {error}"
                ))
            })?;
            return Ok(RunCancellationRequest::Terminal(status));
        }
        let run_owner: String = run
            .try_get("owner_instance_id")
            .map_err(|error| IronCrewError::Validation(format!("Column error: {error}")))?;

        let key_sql = format!(
            "SELECT key_hash FROM {} \
             WHERE operation = $1 AND scope = $2 AND resource_id = $3 \
               AND state IN ('claimed', 'running') \
             ORDER BY created_at DESC LIMIT 2",
            self.idempotency_table
        );
        let keys: Vec<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(key_sql))
            .bind(RUN_OPERATION)
            .bind(flow)
            .bind(run_id)
            .fetch_all(&mut *tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL cancellation ledger lookup failed: {error}"
                ))
            })?;
        let [key_hash] = keys.as_slice() else {
            if keys.len() > 1 {
                return Err(IronCrewError::Conflict(format!(
                    "Run '{run_id}' has multiple active idempotency ledgers"
                )));
            }
            tx.commit().await.map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL non-durable cancellation lookup commit failed: {error}"
                ))
            })?;
            return Ok(RunCancellationRequest::NotDurable);
        };

        self.lock_idempotency_key(&mut tx, key_hash).await?;
        let ledger_sql = format!(
            "SELECT owner_instance_id, state, cancel_requested_at, owner_draining_at FROM {} \
             WHERE key_hash = $1 FOR UPDATE",
            self.idempotency_table
        );
        let Some(ledger) = sqlx::query(sqlx::AssertSqlSafe(ledger_sql))
            .bind(key_hash)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL cancellation ledger fence failed: {error}"
                ))
            })?
        else {
            tx.rollback().await.map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL vanished cancellation ledger rollback failed: {error}"
                ))
            })?;
            return Ok(RunCancellationRequest::NotDurable);
        };
        let ledger_owner: String = ledger
            .try_get("owner_instance_id")
            .map_err(|error| IronCrewError::Validation(format!("Column error: {error}")))?;
        let ledger_state = ledger
            .try_get::<String, _>("state")
            .map_err(|error| IronCrewError::Validation(format!("Column error: {error}")))?
            .parse::<IdempotencyState>()?;
        if ledger_owner != run_owner || !ledger_state.is_in_flight() {
            tx.rollback().await.map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL changed cancellation fence rollback failed: {error}"
                ))
            })?;
            return Ok(RunCancellationRequest::NotDurable);
        }
        let owner_draining = ledger
            .try_get::<Option<String>, _>("owner_draining_at")
            .map_err(|error| IronCrewError::Validation(format!("Column error: {error}")))?
            .is_some();
        if owner_draining {
            tx.commit().await.map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL draining-owner cancellation commit failed: {error}"
                ))
            })?;
            return Ok(RunCancellationRequest::OwnerDraining {
                owner_instance_id: run_owner,
            });
        }
        let already_requested = ledger
            .try_get::<Option<String>, _>("cancel_requested_at")
            .map_err(|error| IronCrewError::Validation(format!("Column error: {error}")))?
            .is_some();
        let (database_now, _) = self
            .database_clock_with_deadline(&mut tx, 0, "run cancellation request")
            .await?;
        let update_sql = format!(
            "UPDATE {} SET \
                 cancel_requested_at = COALESCE(cancel_requested_at, $1), \
                 updated_at = CASE WHEN cancel_requested_at IS NULL THEN $1 ELSE updated_at END \
             WHERE key_hash = $2 AND owner_instance_id = $3 \
               AND state IN ('claimed', 'running')",
            self.idempotency_table
        );
        let changed = sqlx::query(sqlx::AssertSqlSafe(update_sql))
            .bind(&database_now)
            .bind(key_hash)
            .bind(&run_owner)
            .execute(&mut *tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL cancellation request failed: {error}"
                ))
            })?;
        if changed.rows_affected() != 1 {
            tx.rollback().await.map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL cancellation race rollback failed: {error}"
                ))
            })?;
            return Ok(RunCancellationRequest::NotDurable);
        }
        self.delete_human_inputs_for_run(&mut tx, run_id).await?;
        tx.commit().await.map_err(|error| {
            IronCrewError::Validation(format!(
                "PostgreSQL cancellation request commit failed: {error}"
            ))
        })?;
        Ok(RunCancellationRequest::Requested {
            owner_instance_id: run_owner,
            already_requested,
        })
    }
}
