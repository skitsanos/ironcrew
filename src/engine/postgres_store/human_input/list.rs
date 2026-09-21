use sqlx::Row;

use crate::engine::human_input::{DurableHumanInputQuestion, HumanInputAad, HumanInputListOutcome};
use crate::engine::idempotency::RUN_OPERATION;
use crate::utils::error::{IronCrewError, Result};

use super::super::{HUMAN_INPUT_READ_CONCURRENCY_ENV, PostgresStore};
use super::validate_human_input_route;

impl PostgresStore {
    pub(in crate::engine::postgres_store) async fn list_human_input_records(
        &self,
        flow: &str,
        run_id: &str,
    ) -> Result<HumanInputListOutcome> {
        validate_human_input_route("flow", flow, 255)?;
        validate_human_input_route("run id", run_id, 128)?;
        let Some(keyring) = self.human_input_keyring.as_ref() else {
            return Ok(HumanInputListOutcome::NotDurable);
        };
        let _read_permit = self.human_input_read_slots.try_acquire().map_err(|_| {
            IronCrewError::Conflict(format!(
                "PostgreSQL human-input read concurrency is exhausted; raise \
                 {HUMAN_INPUT_READ_CONCURRENCY_ENV} if the pod has sufficient memory"
            ))
        })?;

        let owner_sql = format!(
            "SELECT run.owner_instance_id FROM {runs} AS run \
             JOIN {idempotency} AS idem \
               ON idem.operation = $1 AND idem.scope = run.flow \
              AND idem.resource_id = run.run_id \
              AND idem.owner_instance_id = run.owner_instance_id \
             WHERE run.run_id = $2 AND run.flow = $3 \
               AND run.status IN ('running', 'waiting_for_input') \
               AND run.lease_expires_at <> '' \
               AND run.lease_expires_at::timestamptz > clock_timestamp() \
               AND idem.state = 'running' \
               AND idem.lease_expires_at <> '' \
               AND idem.lease_expires_at::timestamptz > clock_timestamp() \
             ORDER BY idem.created_at DESC LIMIT 2",
            runs = self.table_name,
            idempotency = self.idempotency_table,
        );
        let owners: Vec<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(owner_sql))
            .bind(RUN_OPERATION)
            .bind(run_id)
            .bind(flow)
            .fetch_all(&self.pool)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL human-input owner lookup failed: {error}"
                ))
            })?;
        let [owner_instance_id] = owners.as_slice() else {
            if owners.len() > 1 {
                return Err(IronCrewError::Conflict(format!(
                    "Run '{run_id}' has multiple active keyed attempts"
                )));
            }
            return Ok(HumanInputListOutcome::NotDurable);
        };

        // Size metadata first so a corrupted or externally-written mailbox
        // cannot force the process to materialize an unbounded encrypted row
        // set. The subsequent query still fetches one sentinel row and
        // re-checks cumulative bytes to fail closed across concurrent inserts.
        let list_limits_sql = format!(
            "SELECT COUNT(*)::BIGINT, \
                    COALESCE(SUM(octet_length(human.question_nonce) + \
                                 octet_length(human.question_ciphertext)), 0)::BIGINT \
             FROM {human_inputs} AS human \
             JOIN {runs} AS run ON run.run_id = human.run_id \
             JOIN {idempotency} AS idem \
               ON idem.key_hash = human.key_hash \
              AND idem.attempt_id = human.attempt_id \
              AND idem.owner_instance_id = human.owner_instance_id \
              AND idem.operation = $1 AND idem.scope = human.flow \
              AND idem.resource_id = human.run_id \
             WHERE human.run_id = $2 AND human.flow = $3 \
               AND human.state = 'pending' \
               AND human.expires_at > clock_timestamp() \
               AND run.status IN ('running', 'waiting_for_input') \
               AND run.owner_instance_id = human.owner_instance_id \
               AND run.lease_expires_at <> '' \
               AND run.lease_expires_at::timestamptz > clock_timestamp() \
               AND idem.state = 'running' \
               AND idem.lease_expires_at <> '' \
               AND idem.lease_expires_at::timestamptz > clock_timestamp()",
            human_inputs = self.human_inputs_table,
            runs = self.table_name,
            idempotency = self.idempotency_table,
        );
        let (pending_rows, pending_ciphertext_bytes): (i64, i64) =
            sqlx::query_as(sqlx::AssertSqlSafe(list_limits_sql))
                .bind(RUN_OPERATION)
                .bind(run_id)
                .bind(flow)
                .fetch_one(&self.pool)
                .await
                .map_err(|error| {
                    IronCrewError::Validation(format!(
                        "PostgreSQL human-input list bounds failed: {error}"
                    ))
                })?;
        let pending_rows = usize::try_from(pending_rows).map_err(|_| {
            IronCrewError::Validation("PostgreSQL human-input pending row count is invalid".into())
        })?;
        let pending_ciphertext_bytes = usize::try_from(pending_ciphertext_bytes).map_err(|_| {
            IronCrewError::Validation(
                "PostgreSQL human-input ciphertext accounting is invalid".into(),
            )
        })?;
        if pending_rows > self.human_input_max_pending_rows
            || pending_ciphertext_bytes > self.human_input_max_pending_ciphertext_bytes
        {
            return Err(IronCrewError::Validation(format!(
                "PostgreSQL human-input mailbox exceeds its configured read bounds ({} rows, {} ciphertext bytes)",
                self.human_input_max_pending_rows, self.human_input_max_pending_ciphertext_bytes,
            )));
        }

        let list_sql = format!(
            "SELECT human.question_id, human.owner_instance_id, human.key_hash, \
                    human.attempt_id, human.question_digest, human.question_key_fingerprint, \
                    human.question_nonce, human.question_ciphertext \
             FROM {human_inputs} AS human \
             JOIN {runs} AS run ON run.run_id = human.run_id \
             JOIN {idempotency} AS idem \
               ON idem.key_hash = human.key_hash \
              AND idem.attempt_id = human.attempt_id \
              AND idem.owner_instance_id = human.owner_instance_id \
              AND idem.operation = $1 AND idem.scope = human.flow \
              AND idem.resource_id = human.run_id \
             WHERE human.run_id = $2 AND human.flow = $3 \
               AND human.state = 'pending' \
               AND human.expires_at > clock_timestamp() \
               AND run.status IN ('running', 'waiting_for_input') \
               AND run.owner_instance_id = human.owner_instance_id \
               AND run.lease_expires_at <> '' \
               AND run.lease_expires_at::timestamptz > clock_timestamp() \
               AND idem.state = 'running' \
               AND idem.lease_expires_at <> '' \
               AND idem.lease_expires_at::timestamptz > clock_timestamp() \
             ORDER BY human.created_at, human.question_id LIMIT {}",
            self.human_input_max_pending_rows + 1,
            human_inputs = self.human_inputs_table,
            runs = self.table_name,
            idempotency = self.idempotency_table,
        );
        let rows = sqlx::query(sqlx::AssertSqlSafe(list_sql))
            .bind(RUN_OPERATION)
            .bind(run_id)
            .bind(flow)
            .fetch_all(&self.pool)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!("PostgreSQL human-input list failed: {error}"))
            })?;
        if rows.len() > self.human_input_max_pending_rows {
            return Err(IronCrewError::Validation(format!(
                "PostgreSQL human-input mailbox exceeds its {}-row configured limit",
                self.human_input_max_pending_rows,
            )));
        }
        let mut questions = Vec::with_capacity(rows.len());
        let mut read_ciphertext_bytes = 0usize;
        for row in rows {
            let question_id: String = row
                .try_get("question_id")
                .map_err(|error| IronCrewError::Validation(format!("Column error: {error}")))?;
            let row_owner: String = row
                .try_get("owner_instance_id")
                .map_err(|error| IronCrewError::Validation(format!("Column error: {error}")))?;
            let key_hash: String = row
                .try_get("key_hash")
                .map_err(|error| IronCrewError::Validation(format!("Column error: {error}")))?;
            let attempt_id: String = row
                .try_get("attempt_id")
                .map_err(|error| IronCrewError::Validation(format!("Column error: {error}")))?;
            let question_digest: String = row
                .try_get("question_digest")
                .map_err(|error| IronCrewError::Validation(format!("Column error: {error}")))?;
            let fingerprint: String = row
                .try_get("question_key_fingerprint")
                .map_err(|error| IronCrewError::Validation(format!("Column error: {error}")))?;
            let nonce: Vec<u8> = row
                .try_get("question_nonce")
                .map_err(|error| IronCrewError::Validation(format!("Column error: {error}")))?;
            let ciphertext: Vec<u8> = row
                .try_get("question_ciphertext")
                .map_err(|error| IronCrewError::Validation(format!("Column error: {error}")))?;
            read_ciphertext_bytes = read_ciphertext_bytes
                .checked_add(nonce.len())
                .and_then(|bytes| bytes.checked_add(ciphertext.len()))
                .ok_or_else(|| {
                    IronCrewError::Validation(
                        "PostgreSQL human-input ciphertext byte count overflow".into(),
                    )
                })?;
            if read_ciphertext_bytes > self.human_input_max_pending_ciphertext_bytes {
                return Err(IronCrewError::Validation(format!(
                    "PostgreSQL human-input mailbox exceeds its {}-byte configured ciphertext limit",
                    self.human_input_max_pending_ciphertext_bytes,
                )));
            }
            let aad = HumanInputAad::new(
                flow,
                run_id,
                &question_id,
                &question_digest,
                &row_owner,
                &key_hash,
                &attempt_id,
            )?;
            let info = keyring.open_question(&aad, &fingerprint, &nonce, &ciphertext)?;
            if info.question_id != question_id || row_owner != *owner_instance_id {
                return Err(IronCrewError::Conflict(
                    "Durable human-input question metadata does not match its routing fence".into(),
                ));
            }
            let question = DurableHumanInputQuestion {
                info,
                owner_instance_id: row_owner,
            };
            question.validate()?;
            questions.push(question);
        }
        Ok(HumanInputListOutcome::Shared {
            owner_instance_id: owner_instance_id.clone(),
            questions,
        })
    }
}
