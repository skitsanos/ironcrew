use crate::engine::idempotency::{IdempotencyRecord, IdempotencyState};
use crate::utils::error::{IronCrewError, Result};

use super::super::PostgresStore;
impl PostgresStore {
    pub(in crate::engine::postgres_store) async fn mark_record_indeterminate_in_transaction(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        mut record: IdempotencyRecord,
    ) -> Result<IdempotencyRecord> {
        let (completed_at, expires_at) = self
            .database_clock_with_deadline(
                tx,
                record.ttl_seconds,
                "idempotency indeterminate transition",
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
            .bind(&completed_at)
            .bind(&expires_at)
            .bind(&record.key_hash)
            .bind(&record.attempt_id)
            .execute(&mut **tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL idempotency indeterminate transition failed: {error}"
                ))
            })?;
        if result.rows_affected() != 1 {
            return Err(IronCrewError::Conflict(format!(
                "Idempotency claim '{}' changed before it could be fenced",
                record.key_hash
            )));
        }
        record.state = IdempotencyState::Indeterminate;
        record.response_status = None;
        record.response_body = None;
        record.lease_expires_at.clear();
        record.updated_at = completed_at.clone();
        record.completed_at = Some(completed_at);
        record.expires_at = Some(expires_at);
        record.validate()?;
        Ok(record)
    }
}
