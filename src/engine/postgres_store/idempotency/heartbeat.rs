use crate::engine::idempotency::{IdempotencyState, validate_digest};
use crate::utils::error::{IronCrewError, Result};

use super::super::PostgresStore;
use super::super::codecs::parse_timestamp;
impl PostgresStore {
    pub(in crate::engine::postgres_store) async fn heartbeat_idempotency_record(
        &self,
        key_hash: &str,
        attempt_id: &str,
        new_lease_expires_at: &str,
    ) -> Result<bool> {
        validate_digest("idempotency key hash", key_hash)?;
        if attempt_id.is_empty() || attempt_id.len() > 128 {
            return Err(IronCrewError::Validation(
                "Idempotency attempt id must be 1..=128 bytes".into(),
            ));
        }
        parse_timestamp("idempotency heartbeat lease expiry", new_lease_expires_at)?;
        let mut tx = self.pool.begin().await.map_err(|error| {
            IronCrewError::Validation(format!(
                "PostgreSQL idempotency heartbeat transaction failed: {error}"
            ))
        })?;
        self.configure_run_lease_transaction(&mut tx).await?;
        self.lock_idempotency_key(&mut tx, key_hash).await?;
        let (database_now, database_deadline) = self
            .database_clock_with_deadline(
                &mut tx,
                self.lease.ttl().as_secs(),
                "idempotency heartbeat",
            )
            .await?;
        let Some(record) = self
            .get_idempotency_in_transaction(&mut tx, key_hash)
            .await?
        else {
            return Ok(false);
        };
        if record.attempt_id != attempt_id {
            return Err(IronCrewError::Conflict(
                "Idempotency attempt changed before heartbeat".into(),
            ));
        }
        if !record.state.is_in_flight() {
            return Ok(record.state == IdempotencyState::Completed);
        }
        let sql = format!(
            "UPDATE {} SET lease_expires_at = $1, updated_at = $2 \
             WHERE key_hash = $3 AND attempt_id = $4 \
               AND state IN ('claimed', 'running')",
            self.idempotency_table
        );
        let result = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(&database_deadline)
            .bind(&database_now)
            .bind(key_hash)
            .bind(attempt_id)
            .execute(&mut *tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL idempotency heartbeat failed: {error}"
                ))
            })?;
        tx.commit().await.map_err(|error| {
            IronCrewError::Validation(format!(
                "PostgreSQL idempotency heartbeat commit failed: {error}"
            ))
        })?;
        Ok(result.rows_affected() == 1)
    }
}
