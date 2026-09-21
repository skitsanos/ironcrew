use sqlx::Row;

use crate::engine::human_input::{DurableHumanInputRegistration, HumanInputReadOutcome};
use crate::engine::idempotency::RUN_OPERATION;
use crate::utils::error::{IronCrewError, Result};

use super::super::PostgresStore;

impl PostgresStore {
    pub(in crate::engine::postgres_store) async fn read_human_input_record(
        &self,
        registration: &DurableHumanInputRegistration,
    ) -> Result<HumanInputReadOutcome> {
        registration.validate()?;
        let Some(keyring) = self.human_input_keyring.as_ref() else {
            return Ok(HumanInputReadOutcome::NotDurable);
        };
        let aad = registration.aad(self.lease.instance_id())?;
        let sql = format!(
            "SELECT human.state, human.answer_key_fingerprint, human.answer_nonce, \
                    human.answer_ciphertext, human.question_digest \
             FROM {human_inputs} AS human \
             JOIN {runs} AS run ON run.run_id = human.run_id \
             JOIN {idempotency} AS idem \
               ON idem.key_hash = human.key_hash \
              AND idem.attempt_id = human.attempt_id \
              AND idem.owner_instance_id = human.owner_instance_id \
              AND idem.operation = $1 AND idem.scope = human.flow \
              AND idem.resource_id = human.run_id \
             WHERE human.run_id = $2 AND human.question_id = $3 \
               AND human.flow = $4 AND human.owner_instance_id = $5 \
               AND human.key_hash = $6 AND human.attempt_id = $7 \
               AND human.question_digest = $8 \
               AND (human.state = 'answered' OR human.expires_at > clock_timestamp()) \
               AND run.owner_instance_id = human.owner_instance_id \
               AND run.status IN ('running', 'waiting_for_input') \
               AND run.lease_expires_at <> '' \
               AND run.lease_expires_at::timestamptz > clock_timestamp() \
               AND idem.state = 'running' \
               AND idem.lease_expires_at <> '' \
               AND idem.lease_expires_at::timestamptz > clock_timestamp()",
            human_inputs = self.human_inputs_table,
            runs = self.table_name,
            idempotency = self.idempotency_table,
        );
        let Some(row) = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(RUN_OPERATION)
            .bind(&registration.run_id)
            .bind(&registration.question.question_id)
            .bind(&registration.flow)
            .bind(self.lease.instance_id())
            .bind(&registration.key_hash)
            .bind(&registration.attempt_id)
            .bind(&aad.question_digest)
            .fetch_optional(&self.pool)
            .await
            .map_err(|error| {
                IronCrewError::Io(std::io::Error::other(format!(
                    "PostgreSQL human-input owner read failed: {error}"
                )))
            })?
        else {
            return Ok(HumanInputReadOutcome::NotFound);
        };
        let question_digest: String = row
            .try_get("question_digest")
            .map_err(|error| IronCrewError::Validation(format!("Column error: {error}")))?;
        if question_digest != aad.question_digest {
            return Err(IronCrewError::Conflict(
                "Durable human-input question digest does not match its registration".into(),
            ));
        }
        let state: String = row
            .try_get("state")
            .map_err(|error| IronCrewError::Validation(format!("Column error: {error}")))?;
        if state == "pending" {
            return Ok(HumanInputReadOutcome::Pending);
        }
        if state != "answered" {
            return Err(IronCrewError::Validation(format!(
                "PostgreSQL human-input row has invalid state '{state}'"
            )));
        }
        let fingerprint: String = row
            .try_get::<Option<String>, _>("answer_key_fingerprint")
            .map_err(|error| IronCrewError::Validation(format!("Column error: {error}")))?
            .ok_or_else(|| {
                IronCrewError::Validation(
                    "Answered PostgreSQL human-input row has no key fingerprint".into(),
                )
            })?;
        let nonce: Vec<u8> = row
            .try_get::<Option<Vec<u8>>, _>("answer_nonce")
            .map_err(|error| IronCrewError::Validation(format!("Column error: {error}")))?
            .ok_or_else(|| {
                IronCrewError::Validation("Answered PostgreSQL human-input row has no nonce".into())
            })?;
        let ciphertext: Vec<u8> = row
            .try_get::<Option<Vec<u8>>, _>("answer_ciphertext")
            .map_err(|error| IronCrewError::Validation(format!("Column error: {error}")))?
            .ok_or_else(|| {
                IronCrewError::Validation(
                    "Answered PostgreSQL human-input row has no ciphertext".into(),
                )
            })?;
        let answer = keyring.open_json(&aad, &fingerprint, &nonce, &ciphertext)?;
        Ok(HumanInputReadOutcome::Answered(answer))
    }
}
