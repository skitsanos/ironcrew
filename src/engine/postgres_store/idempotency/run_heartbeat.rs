use sqlx::Row;

use crate::engine::idempotency::{
    IdempotencyState, RUN_OPERATION, RunFenceHeartbeat, validate_digest,
};
use crate::engine::run_history::RunStatus;
use crate::utils::error::{IronCrewError, Result};

use super::super::PostgresStore;
use super::super::codecs::parse_timestamp;
impl PostgresStore {
    pub(in crate::engine::postgres_store) async fn heartbeat_idempotent_run_record(
        &self,
        run_id: &str,
        key_hash: &str,
        attempt_id: &str,
        new_lease_expires_at: &str,
    ) -> Result<RunFenceHeartbeat> {
        validate_digest("idempotency key hash", key_hash)?;
        if run_id.is_empty() || run_id.len() > 128 {
            return Err(IronCrewError::Validation(
                "Idempotent run id must be 1..=128 bytes".into(),
            ));
        }
        if attempt_id.is_empty() || attempt_id.len() > 128 {
            return Err(IronCrewError::Validation(
                "Idempotency attempt id must be 1..=128 bytes".into(),
            ));
        }
        // The absolute caller deadline remains part of the shared backend
        // contract, but PostgreSQL uses only its own clock for lease ordering.
        parse_timestamp(
            "idempotent run heartbeat lease expiry",
            new_lease_expires_at,
        )?;
        let mut tx = self.pool.begin().await.map_err(|error| {
            IronCrewError::Validation(format!(
                "PostgreSQL idempotent run heartbeat transaction failed: {error}"
            ))
        })?;
        self.configure_run_lease_transaction(&mut tx).await?;
        self.lock_run_fence(&mut tx, true).await?;
        self.lock_resource(&mut tx, RUN_OPERATION, "", run_id)
            .await?;
        self.lock_idempotency_key(&mut tx, key_hash).await?;
        let (database_now, database_deadline) = self
            .database_clock_with_deadline(
                &mut tx,
                self.lease.ttl().as_secs(),
                "idempotent run heartbeat",
            )
            .await?;
        let Some(record) = self
            .get_idempotency_in_transaction(&mut tx, key_hash)
            .await?
        else {
            tx.commit().await.map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL missing run fence heartbeat commit failed: {error}"
                ))
            })?;
            return Ok(RunFenceHeartbeat::Lost);
        };
        if record.attempt_id != attempt_id {
            return Err(IronCrewError::Conflict(
                "Idempotency attempt changed before run heartbeat".into(),
            ));
        }
        if record.operation != RUN_OPERATION
            || record.resource_id != run_id
            || record.owner_instance_id != self.lease.instance_id()
        {
            tx.commit().await.map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL mismatched run fence heartbeat commit failed: {error}"
                ))
            })?;
            return Ok(RunFenceHeartbeat::Lost);
        }
        let cancellation_sql = format!(
            "SELECT cancel_requested_at FROM {} WHERE key_hash = $1",
            self.idempotency_table
        );
        let cancel_requested_at: Option<String> =
            sqlx::query_scalar(sqlx::AssertSqlSafe(cancellation_sql))
                .bind(key_hash)
                .fetch_one(&mut *tx)
                .await
                .map_err(|error| {
                    IronCrewError::Validation(format!(
                        "PostgreSQL idempotent run cancellation lookup failed: {error}"
                    ))
                })?;

        let run_sql = format!(
            "SELECT status, owner_instance_id, flow FROM {} WHERE run_id = $1 FOR UPDATE",
            self.table_name
        );
        let run = sqlx::query(sqlx::AssertSqlSafe(run_sql))
            .bind(run_id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL idempotent heartbeat run lookup failed: {error}"
                ))
            })?;

        let Some(run) = run else {
            if record.state != IdempotencyState::Claimed {
                tx.commit().await.map_err(|error| {
                    IronCrewError::Validation(format!(
                        "PostgreSQL lost run fence heartbeat commit failed: {error}"
                    ))
                })?;
                return Ok(RunFenceHeartbeat::Lost);
            }
            if cancel_requested_at.is_some() {
                tx.commit().await.map_err(|error| {
                    IronCrewError::Validation(format!(
                        "PostgreSQL claimed run cancellation commit failed: {error}"
                    ))
                })?;
                return Ok(RunFenceHeartbeat::CancelRequested);
            }
            let ledger_sql = format!(
                "UPDATE {} SET lease_expires_at = $1, updated_at = $2 \
                 WHERE key_hash = $3 AND operation = $4 AND resource_id = $5 \
                   AND attempt_id = $6 AND owner_instance_id = $7 \
                   AND state = 'claimed'",
                self.idempotency_table
            );
            let renewed = sqlx::query(sqlx::AssertSqlSafe(ledger_sql))
                .bind(&database_deadline)
                .bind(&database_now)
                .bind(key_hash)
                .bind(RUN_OPERATION)
                .bind(run_id)
                .bind(attempt_id)
                .bind(self.lease.instance_id())
                .execute(&mut *tx)
                .await
                .map_err(|error| {
                    IronCrewError::Validation(format!(
                        "PostgreSQL claimed run fence heartbeat failed: {error}"
                    ))
                })?;
            let outcome = if renewed.rows_affected() == 1 {
                RunFenceHeartbeat::Owned
            } else {
                RunFenceHeartbeat::Lost
            };
            tx.commit().await.map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL claimed run fence heartbeat commit failed: {error}"
                ))
            })?;
            return Ok(outcome);
        };

        let run_owner: String = run
            .try_get("owner_instance_id")
            .map_err(|error| IronCrewError::Validation(format!("Column error: {error}")))?;
        let run_flow: String = run
            .try_get("flow")
            .map_err(|error| IronCrewError::Validation(format!("Column error: {error}")))?;
        if run_owner != self.lease.instance_id() || run_flow != record.scope {
            tx.commit().await.map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL unowned run fence heartbeat commit failed: {error}"
                ))
            })?;
            return Ok(RunFenceHeartbeat::Lost);
        }
        let status = run
            .try_get::<String, _>("status")
            .map_err(|error| IronCrewError::Validation(format!("Column error: {error}")))?
            .parse::<RunStatus>()?;
        if status.is_terminal() {
            self.delete_human_inputs_for_run(&mut tx, run_id).await?;
            tx.commit().await.map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL terminal run fence heartbeat commit failed: {error}"
                ))
            })?;
            return Ok(RunFenceHeartbeat::Terminal(status));
        }
        if record.state != IdempotencyState::Running || !status.is_in_flight() {
            tx.commit().await.map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL lost run fence heartbeat commit failed: {error}"
                ))
            })?;
            return Ok(RunFenceHeartbeat::Lost);
        }
        if cancel_requested_at.is_some() {
            tx.commit().await.map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL running cancellation request commit failed: {error}"
                ))
            })?;
            return Ok(RunFenceHeartbeat::CancelRequested);
        }

        let run_update_sql = format!(
            "UPDATE {} SET lease_expires_at = $1 \
             WHERE run_id = $2 AND owner_instance_id = $3 \
               AND status IN ('running', 'waiting_for_input')",
            self.table_name
        );
        let run_renewed = sqlx::query(sqlx::AssertSqlSafe(run_update_sql))
            .bind(&database_deadline)
            .bind(run_id)
            .bind(self.lease.instance_id())
            .execute(&mut *tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL idempotent run lease heartbeat failed: {error}"
                ))
            })?;
        let ledger_update_sql = format!(
            "UPDATE {} SET lease_expires_at = $1, updated_at = $2 \
             WHERE key_hash = $3 AND operation = $4 AND resource_id = $5 \
               AND attempt_id = $6 AND owner_instance_id = $7 \
               AND state = 'running'",
            self.idempotency_table
        );
        let ledger_renewed = sqlx::query(sqlx::AssertSqlSafe(ledger_update_sql))
            .bind(&database_deadline)
            .bind(&database_now)
            .bind(key_hash)
            .bind(RUN_OPERATION)
            .bind(run_id)
            .bind(attempt_id)
            .bind(self.lease.instance_id())
            .execute(&mut *tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL idempotent run ledger heartbeat failed: {error}"
                ))
            })?;
        if run_renewed.rows_affected() != 1 || ledger_renewed.rows_affected() != 1 {
            tx.rollback().await.map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL lost run fence heartbeat rollback failed: {error}"
                ))
            })?;
            return Ok(RunFenceHeartbeat::Lost);
        }
        tx.commit().await.map_err(|error| {
            IronCrewError::Validation(format!(
                "PostgreSQL idempotent run heartbeat commit failed: {error}"
            ))
        })?;
        Ok(RunFenceHeartbeat::Owned)
    }
}
