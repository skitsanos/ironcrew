use crate::engine::idempotency::{
    IdempotencyCompletion, IdempotencyCompletionOutcome, IdempotencyLimits, IdempotencyState,
};
use crate::utils::error::{IronCrewError, Result};

use super::super::PostgresStore;
impl PostgresStore {
    pub(in crate::engine::postgres_store) async fn complete_idempotency_record(
        &self,
        completion: IdempotencyCompletion,
        limits: IdempotencyLimits,
    ) -> Result<IdempotencyCompletionOutcome> {
        completion.validate()?;
        limits.validate()?;
        let mut tx = self.pool.begin().await.map_err(|error| {
            IronCrewError::Validation(format!(
                "PostgreSQL idempotency completion transaction failed: {error}"
            ))
        })?;
        self.lock_idempotency_quota(&mut tx).await?;
        self.lock_idempotency_principal(&mut tx, &completion.principal_id)
            .await?;
        self.lock_idempotency_key(&mut tx, &completion.key_hash)
            .await?;
        let record = self
            .get_idempotency_in_transaction(&mut tx, &completion.key_hash)
            .await?
            .ok_or_else(|| {
                IronCrewError::Validation("Idempotency claim not found during completion".into())
            })?;
        if record.principal_id != completion.principal_id
            || record.request_fingerprint != completion.request_fingerprint
            || record.attempt_id != completion.attempt_id
            || record.owner_instance_id != completion.owner_instance_id
        {
            return Err(IronCrewError::Conflict(format!(
                "Idempotency claim '{}' is fenced by a different attempt",
                completion.key_hash
            )));
        }
        if record.state == IdempotencyState::Completed {
            let outcome = IdempotencyCompletionOutcome {
                replayable: record.replayable(),
                already_completed: true,
            };
            tx.commit().await.map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL idempotency completion commit failed: {error}"
                ))
            })?;
            return Ok(outcome);
        }
        if record.state == IdempotencyState::Indeterminate {
            return Err(IronCrewError::Conflict(
                "Indeterminate idempotency outcomes cannot be completed".into(),
            ));
        }
        let (database_completed_at, database_expires_at) = self
            .database_clock_with_deadline(&mut tx, record.ttl_seconds, "idempotency completion")
            .await?;

        let (global_usage, principal_usage) = self
            .idempotency_accounting_for_update(&mut tx, &completion.principal_id)
            .await?;
        let old_response_bytes = record.response_body.as_ref().map_or(0, String::len);
        let global_without_record = global_usage
            .response_bytes
            .checked_sub(old_response_bytes)
            .ok_or_else(|| {
                IronCrewError::Validation(
                    "PostgreSQL global idempotency response accounting is inconsistent".into(),
                )
            })?;
        let principal_without_record = principal_usage
            .response_bytes
            .checked_sub(old_response_bytes)
            .ok_or_else(|| {
                IronCrewError::Validation(
                    "PostgreSQL principal idempotency response accounting is inconsistent".into(),
                )
            })?;
        let response_body = completion.response_body.as_ref().filter(|body| {
            global_without_record
                .checked_add(body.len())
                .is_some_and(|total| total <= limits.global_max_response_bytes)
                && principal_without_record
                    .checked_add(body.len())
                    .is_some_and(|total| total <= limits.principal_max_response_bytes)
        });
        let sql = format!(
            "UPDATE {} SET state = 'completed', response_status = $1, \
             response_body = $2, lease_expires_at = '', updated_at = $3, \
             completed_at = $3, expires_at = $4 \
             WHERE key_hash = $5 AND request_fingerprint = $6 \
               AND attempt_id = $7 AND owner_instance_id = $8 \
               AND state IN ('claimed', 'running')",
            self.idempotency_table
        );
        let result = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(i32::from(completion.response_status))
            .bind(response_body)
            .bind(&database_completed_at)
            .bind(&database_expires_at)
            .bind(&completion.key_hash)
            .bind(&completion.request_fingerprint)
            .bind(&completion.attempt_id)
            .bind(&completion.owner_instance_id)
            .execute(&mut *tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL idempotency completion update failed: {error}"
                ))
            })?;
        if result.rows_affected() != 1 {
            return Err(IronCrewError::Conflict(format!(
                "Idempotency claim '{}' changed before completion",
                completion.key_hash
            )));
        }
        tx.commit().await.map_err(|error| {
            IronCrewError::Validation(format!(
                "PostgreSQL idempotency completion commit failed: {error}"
            ))
        })?;
        Ok(IdempotencyCompletionOutcome {
            replayable: response_body.is_some(),
            already_completed: false,
        })
    }
}
