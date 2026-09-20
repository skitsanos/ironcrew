use crate::engine::human_input::{HumanInputAad, HumanInputAnswerOutcome, validate_durable_answer};
use crate::engine::idempotency::RUN_OPERATION;
use crate::utils::error::{IronCrewError, Result};

use super::super::{HUMAN_INPUT_READ_CONCURRENCY_ENV, PostgresStore};
use super::validate_human_input_route;

impl PostgresStore {
    pub(in crate::engine::postgres_store) async fn answer_human_input_record(
        &self,
        flow: &str,
        run_id: &str,
        question_id: &str,
        answer: &serde_json::Value,
    ) -> Result<HumanInputAnswerOutcome> {
        validate_human_input_route("flow", flow, 255)?;
        validate_human_input_route("run id", run_id, 128)?;
        validate_human_input_route("question id", question_id, 128)?;
        validate_durable_answer(answer)?;
        let Some(keyring) = self.human_input_keyring.as_ref() else {
            return Ok(HumanInputAnswerOutcome::NotDurable);
        };
        let _read_permit = self.human_input_read_slots.try_acquire().map_err(|_| {
            IronCrewError::Conflict(format!(
                "PostgreSQL human-input read concurrency is exhausted; raise \
                 {HUMAN_INPUT_READ_CONCURRENCY_ENV} if the pod has sufficient memory"
            ))
        })?;

        let mut tx = self.pool.begin().await.map_err(|error| {
            IronCrewError::Validation(format!(
                "PostgreSQL human-input answer transaction failed: {error}"
            ))
        })?;
        self.lock_run_fence(&mut tx, true).await?;
        self.lock_resource(&mut tx, RUN_OPERATION, "", run_id)
            .await?;
        let Some(row) = self
            .load_bounded_human_input_answer_row(&mut tx, flow, run_id, question_id)
            .await?
        else {
            tx.commit().await.map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL missing human-input answer commit failed: {error}"
                ))
            })?;
            return Ok(HumanInputAnswerOutcome::NotFound);
        };
        let super::answer_row::HumanInputAnswerRow {
            owner_instance_id,
            key_hash,
            attempt_id,
            question_digest,
            question_key_fingerprint,
            question_nonce,
            question_ciphertext,
            state,
        } = row;
        self.lock_idempotency_key(&mut tx, &key_hash).await?;
        let (database_now, _) = self
            .database_clock_with_deadline(&mut tx, 0, "human-input answer")
            .await?;
        let drain_sql = format!(
            "SELECT owner_draining_at FROM {} \
             WHERE key_hash = $1 AND attempt_id = $2 AND owner_instance_id = $3 \
               AND operation = $4 AND scope = $5 AND resource_id = $6 \
               AND state = 'running' FOR UPDATE",
            self.idempotency_table
        );
        let draining: Option<Option<String>> = sqlx::query_scalar(sqlx::AssertSqlSafe(drain_sql))
            .bind(&key_hash)
            .bind(&attempt_id)
            .bind(&owner_instance_id)
            .bind(RUN_OPERATION)
            .bind(flow)
            .bind(run_id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL human-input owner-drain lookup failed: {error}"
                ))
            })?;
        if draining.as_ref().is_some_and(Option::is_some) {
            tx.commit().await.map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL draining-owner human-input commit failed: {error}"
                ))
            })?;
            return Ok(HumanInputAnswerOutcome::OwnerDraining { owner_instance_id });
        }
        let active_sql = format!(
            "SELECT EXISTS (\
                 SELECT 1 FROM {human_inputs} AS human \
                 JOIN {runs} AS run ON run.run_id = human.run_id \
                 JOIN {idempotency} AS idem \
                   ON idem.key_hash = human.key_hash \
                  AND idem.attempt_id = human.attempt_id \
                  AND idem.owner_instance_id = human.owner_instance_id \
                  AND idem.operation = $1 AND idem.scope = human.flow \
                  AND idem.resource_id = human.run_id \
                 WHERE human.run_id = $2 AND human.question_id = $3 \
                   AND human.flow = $4 \
                   AND (human.state = 'answered' OR human.expires_at > $5::timestamptz) \
                   AND run.status IN ('running', 'waiting_for_input') \
                   AND run.owner_instance_id = human.owner_instance_id \
                   AND run.lease_expires_at <> '' \
                   AND run.lease_expires_at::timestamptz > $5::timestamptz \
                   AND idem.state = 'running' \
                   AND idem.owner_draining_at IS NULL \
                   AND idem.lease_expires_at <> '' \
                   AND idem.lease_expires_at::timestamptz > $5::timestamptz\
             )",
            human_inputs = self.human_inputs_table,
            runs = self.table_name,
            idempotency = self.idempotency_table,
        );
        let active: bool = sqlx::query_scalar(sqlx::AssertSqlSafe(active_sql))
            .bind(RUN_OPERATION)
            .bind(run_id)
            .bind(question_id)
            .bind(flow)
            .bind(&database_now)
            .fetch_one(&mut *tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL human-input answer fence check failed: {error}"
                ))
            })?;
        if !active {
            let delete_sql = format!(
                "DELETE FROM {} WHERE run_id = $1 AND question_id = $2",
                self.human_inputs_table
            );
            sqlx::query(sqlx::AssertSqlSafe(delete_sql))
                .bind(run_id)
                .bind(question_id)
                .execute(&mut *tx)
                .await
                .map_err(|error| {
                    IronCrewError::Validation(format!(
                        "PostgreSQL stale human-input cleanup failed: {error}"
                    ))
                })?;
            tx.commit().await.map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL stale human-input answer commit failed: {error}"
                ))
            })?;
            return Ok(HumanInputAnswerOutcome::NotFound);
        }
        if state != "pending" && state != "answered" {
            return Err(IronCrewError::Validation(format!(
                "PostgreSQL human-input row has invalid state '{state}'"
            )));
        }

        let aad = HumanInputAad::new(
            flow,
            run_id,
            question_id,
            &question_digest,
            &owner_instance_id,
            &key_hash,
            &attempt_id,
        )?;
        let question = keyring.open_question(
            &aad,
            &question_key_fingerprint,
            &question_nonce,
            &question_ciphertext,
        )?;
        if question.question_id != question_id {
            return Err(IronCrewError::Conflict(
                "Durable human-input question metadata does not match its routing fence".into(),
            ));
        }
        if state == "answered" {
            tx.commit().await.map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL duplicate human-input answer commit failed: {error}"
                ))
            })?;
            return Ok(HumanInputAnswerOutcome::AlreadyAnswered);
        }
        let encrypted =
            keyring.seal_json_for_fingerprint(&aad, answer, &question_key_fingerprint)?;
        let update_sql = format!(
            "UPDATE {human_inputs} AS human SET \
                 answer_key_fingerprint = $1, answer_nonce = $2, answer_ciphertext = $3, \
                 state = 'answered', answered_at = $4::timestamptz \
             WHERE human.run_id = $5 AND human.question_id = $6 AND human.flow = $7 \
               AND human.owner_instance_id = $8 AND human.key_hash = $9 \
               AND human.attempt_id = $10 AND human.question_digest = $11 \
               AND human.state = 'pending' \
               AND human.expires_at > $4::timestamptz \
               AND EXISTS (SELECT 1 FROM {runs} AS run \
                   WHERE run.run_id = human.run_id \
                     AND run.owner_instance_id = human.owner_instance_id \
                     AND run.status IN ('running', 'waiting_for_input') \
                     AND run.lease_expires_at <> '' \
                     AND run.lease_expires_at::timestamptz > $4::timestamptz) \
               AND EXISTS (SELECT 1 FROM {idempotency} AS idem \
                   WHERE idem.key_hash = human.key_hash \
                     AND idem.attempt_id = human.attempt_id \
                     AND idem.owner_instance_id = human.owner_instance_id \
                     AND idem.operation = $12 AND idem.scope = human.flow \
                     AND idem.resource_id = human.run_id AND idem.state = 'running' \
                     AND idem.owner_draining_at IS NULL \
                     AND idem.lease_expires_at <> '' \
                     AND idem.lease_expires_at::timestamptz > $4::timestamptz)",
            human_inputs = self.human_inputs_table,
            runs = self.table_name,
            idempotency = self.idempotency_table,
        );
        let updated = sqlx::query(sqlx::AssertSqlSafe(update_sql))
            .bind(&encrypted.key_fingerprint)
            .bind(&encrypted.nonce)
            .bind(&encrypted.ciphertext)
            .bind(&database_now)
            .bind(run_id)
            .bind(question_id)
            .bind(flow)
            .bind(&owner_instance_id)
            .bind(&key_hash)
            .bind(&attempt_id)
            .bind(&question_digest)
            .bind(RUN_OPERATION)
            .execute(&mut *tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL human-input answer update failed: {error}"
                ))
            })?;
        if updated.rows_affected() != 1 {
            tx.rollback().await.map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL human-input answer race rollback failed: {error}"
                ))
            })?;
            return Ok(HumanInputAnswerOutcome::NotFound);
        }
        tx.commit().await.map_err(|error| {
            IronCrewError::Validation(format!(
                "PostgreSQL human-input answer commit failed: {error}"
            ))
        })?;
        Ok(HumanInputAnswerOutcome::Queued { owner_instance_id })
    }
}
