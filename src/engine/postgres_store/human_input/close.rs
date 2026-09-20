use crate::engine::human_input::{DurableHumanInputRegistration, question_digest};
use crate::engine::idempotency::RUN_OPERATION;
use crate::utils::error::{IronCrewError, Result};

use super::super::PostgresStore;

impl PostgresStore {
    pub(in crate::engine::postgres_store) async fn close_human_input_record(
        &self,
        registration: &DurableHumanInputRegistration,
    ) -> Result<bool> {
        registration.validate()?;
        if self.human_input_keyring.is_none() {
            return Ok(false);
        }
        let mut tx = self.pool.begin().await.map_err(|error| {
            IronCrewError::Validation(format!(
                "PostgreSQL human-input close transaction failed: {error}"
            ))
        })?;
        self.lock_run_fence(&mut tx, true).await?;
        self.lock_resource(&mut tx, RUN_OPERATION, "", &registration.run_id)
            .await?;
        let expected_question_digest = question_digest(&registration.question)?;
        let sql = format!(
            "DELETE FROM {} WHERE run_id = $1 AND question_id = $2 AND flow = $3 \
               AND owner_instance_id = $4 AND key_hash = $5 AND attempt_id = $6 \
               AND question_digest = $7",
            self.human_inputs_table
        );
        let deleted = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(&registration.run_id)
            .bind(&registration.question.question_id)
            .bind(&registration.flow)
            .bind(self.lease.instance_id())
            .bind(&registration.key_hash)
            .bind(&registration.attempt_id)
            .bind(&expected_question_digest)
            .execute(&mut *tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!("PostgreSQL human-input close failed: {error}"))
            })?;
        tx.commit().await.map_err(|error| {
            IronCrewError::Validation(format!(
                "PostgreSQL human-input close commit failed: {error}"
            ))
        })?;
        Ok(deleted.rows_affected() == 1)
    }
}
