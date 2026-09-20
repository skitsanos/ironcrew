use crate::engine::idempotency::validate_digest;
use crate::utils::error::{IronCrewError, Result};

use super::super::PostgresStore;
use super::super::codecs::parse_timestamp;
impl PostgresStore {
    pub(in crate::engine::postgres_store) async fn mark_idempotency_indeterminate_record(
        &self,
        key_hash: &str,
        attempt_id: &str,
        completed_at: &str,
        expires_at: &str,
    ) -> Result<bool> {
        validate_digest("idempotency key hash", key_hash)?;
        parse_timestamp("idempotency completion time", completed_at)?;
        parse_timestamp("idempotency retention expiry", expires_at)?;
        let Some(principal_id) = self.idempotency_principal_for_key(key_hash).await? else {
            return Ok(false);
        };
        let mut tx = self.pool.begin().await.map_err(|error| {
            IronCrewError::Validation(format!(
                "PostgreSQL idempotency indeterminate transaction failed: {error}"
            ))
        })?;
        self.lock_idempotency_quota(&mut tx).await?;
        self.lock_idempotency_principal(&mut tx, &principal_id)
            .await?;
        self.lock_idempotency_key(&mut tx, key_hash).await?;
        let Some(record) = self
            .get_idempotency_in_transaction(&mut tx, key_hash)
            .await?
        else {
            return Ok(false);
        };
        if record.principal_id != principal_id {
            return Err(IronCrewError::Conflict(
                "Idempotency principal changed before indeterminate transition".into(),
            ));
        }
        if record.attempt_id != attempt_id {
            return Err(IronCrewError::Conflict(
                "Idempotency attempt changed before indeterminate transition".into(),
            ));
        }
        if record.state.is_terminal() {
            return Ok(false);
        }
        let (database_completed_at, database_expires_at) = self
            .database_clock_with_deadline(
                &mut tx,
                record.ttl_seconds,
                "idempotency indeterminate completion",
            )
            .await?;
        let sql = format!(
            "UPDATE {} SET state = 'indeterminate', response_status = NULL, \
             response_body = NULL, lease_expires_at = '', updated_at = $1, \
             completed_at = $1, expires_at = $2 \
             WHERE key_hash = $3 AND attempt_id = $4 \
               AND state IN ('claimed', 'running')",
            self.idempotency_table
        );
        let result = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(&database_completed_at)
            .bind(&database_expires_at)
            .bind(key_hash)
            .bind(attempt_id)
            .execute(&mut *tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL idempotency indeterminate update failed: {error}"
                ))
            })?;
        tx.commit().await.map_err(|error| {
            IronCrewError::Validation(format!(
                "PostgreSQL idempotency indeterminate commit failed: {error}"
            ))
        })?;
        Ok(result.rows_affected() == 1)
    }

    pub(in crate::engine::postgres_store) async fn release_idempotency_record(
        &self,
        key_hash: &str,
        attempt_id: &str,
    ) -> Result<bool> {
        validate_digest("idempotency key hash", key_hash)?;
        let Some(principal_id) = self.idempotency_principal_for_key(key_hash).await? else {
            return Ok(false);
        };
        let mut tx = self.pool.begin().await.map_err(|error| {
            IronCrewError::Validation(format!(
                "PostgreSQL idempotency release transaction failed: {error}"
            ))
        })?;
        self.lock_idempotency_quota(&mut tx).await?;
        self.lock_idempotency_principal(&mut tx, &principal_id)
            .await?;
        self.lock_idempotency_key(&mut tx, key_hash).await?;
        let Some(record) = self
            .get_idempotency_in_transaction(&mut tx, key_hash)
            .await?
        else {
            return Ok(false);
        };
        if record.principal_id != principal_id {
            return Err(IronCrewError::Conflict(
                "Idempotency principal changed before release".into(),
            ));
        }
        if record.attempt_id != attempt_id {
            return Err(IronCrewError::Conflict(
                "Idempotency attempt changed before release".into(),
            ));
        }
        if !record.state.is_in_flight() {
            return Ok(false);
        }
        let sql = format!(
            "DELETE FROM {} WHERE key_hash = $1 AND attempt_id = $2 \
             AND state IN ('claimed', 'running')",
            self.idempotency_table
        );
        let result = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(key_hash)
            .bind(attempt_id)
            .execute(&mut *tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!("PostgreSQL idempotency release failed: {error}"))
            })?;
        tx.commit().await.map_err(|error| {
            IronCrewError::Validation(format!(
                "PostgreSQL idempotency release commit failed: {error}"
            ))
        })?;
        Ok(result.rows_affected() == 1)
    }

    pub(in crate::engine::postgres_store) async fn prune_idempotency_records(
        &self,
        now: &str,
        limit: usize,
    ) -> Result<usize> {
        parse_timestamp("idempotency prune time", now)?;
        if limit == 0 {
            return Ok(0);
        }
        let mut tx = self.pool.begin().await.map_err(|error| {
            IronCrewError::Validation(format!(
                "PostgreSQL idempotency prune transaction failed: {error}"
            ))
        })?;
        // Startup invokes this through the same outer watchdog used by lease
        // maintenance. Keep the transaction itself bounded as well so a
        // quota-lock holder is cancelled inside PostgreSQL before the outer
        // future has to drop the transaction.
        self.configure_run_lease_transaction(&mut tx).await?;
        self.lock_idempotency_quota(&mut tx).await?;
        let (database_now, _) = self
            .database_clock_with_deadline(&mut tx, 0, "idempotency pruning")
            .await?;
        let sql = format!(
            "DELETE FROM {table} WHERE key_hash IN (\
                 SELECT key_hash FROM {table} \
                 WHERE state IN ('completed', 'indeterminate') \
                   AND expires_at IS NOT NULL \
                   AND expires_at::timestamptz <= $1::timestamptz \
                 ORDER BY expires_at::timestamptz, key_hash LIMIT $2\
             )",
            table = self.idempotency_table
        );
        let result = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(&database_now)
            .bind(i64::try_from(limit).unwrap_or(i64::MAX))
            .execute(&mut *tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!("PostgreSQL idempotency prune failed: {error}"))
            })?;
        tx.commit().await.map_err(|error| {
            IronCrewError::Validation(format!(
                "PostgreSQL idempotency prune commit failed: {error}"
            ))
        })?;
        Ok(result.rows_affected() as usize)
    }
}
