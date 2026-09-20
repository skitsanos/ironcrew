use crate::engine::human_input::{DurableHumanInputRegistration, HumanInputRegistrationOutcome};
use crate::engine::idempotency::RUN_OPERATION;
use crate::utils::error::{IronCrewError, Result};

use super::super::PostgresStore;

impl PostgresStore {
    pub(in crate::engine::postgres_store) async fn register_human_input_record(
        &self,
        registration: &DurableHumanInputRegistration,
    ) -> Result<HumanInputRegistrationOutcome> {
        registration.validate()?;
        let Some(keyring) = self.human_input_keyring.as_ref() else {
            return Ok(HumanInputRegistrationOutcome::NotDurable);
        };
        let aad = registration.aad(self.lease.instance_id())?;
        let encrypted = keyring.seal_question(&aad, &registration.question)?;

        let mut tx = self.pool.begin().await.map_err(|error| {
            IronCrewError::Validation(format!(
                "PostgreSQL human-input registration transaction failed: {error}"
            ))
        })?;
        self.lock_run_fence(&mut tx, true).await?;
        self.lock_resource(&mut tx, RUN_OPERATION, "", &registration.run_id)
            .await?;
        self.lock_idempotency_key(&mut tx, &registration.key_hash)
            .await?;
        let (database_now, expires_at) = self
            .database_clock_with_deadline(
                &mut tx,
                registration.question.timeout_s,
                "human-input registration",
            )
            .await?;

        let drain_sql = format!(
            "SELECT owner_draining_at FROM {} \
             WHERE key_hash = $1 AND attempt_id = $2 AND owner_instance_id = $3 \
               AND operation = $4 AND scope = $5 AND resource_id = $6 \
               AND state = 'running' FOR UPDATE",
            self.idempotency_table
        );
        let draining: Option<Option<String>> = sqlx::query_scalar(sqlx::AssertSqlSafe(drain_sql))
            .bind(&registration.key_hash)
            .bind(&registration.attempt_id)
            .bind(self.lease.instance_id())
            .bind(RUN_OPERATION)
            .bind(&registration.flow)
            .bind(&registration.run_id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL human-input registration owner-drain lookup failed: {error}"
                ))
            })?;
        if draining.as_ref().is_some_and(Option::is_some) {
            tx.commit().await.map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL draining-owner human-input registration commit failed: {error}"
                ))
            })?;
            return Ok(HumanInputRegistrationOutcome::OwnerDraining {
                owner_instance_id: self.lease.instance_id().to_string(),
            });
        }

        let fence_sql = format!(
            "SELECT EXISTS (\
                 SELECT 1 FROM {runs} AS run \
                 JOIN {idempotency} AS idem \
                   ON idem.operation = $1 AND idem.scope = run.flow \
                  AND idem.resource_id = run.run_id \
                 WHERE run.run_id = $2 AND run.flow = $3 \
                   AND run.owner_instance_id = $4 \
                   AND run.status IN ('running', 'waiting_for_input') \
                   AND run.lease_expires_at <> '' \
                   AND run.lease_expires_at::timestamptz > $5::timestamptz \
                   AND idem.key_hash = $6 AND idem.attempt_id = $7 \
                   AND idem.owner_instance_id = run.owner_instance_id \
                   AND idem.state = 'running' \
                   AND idem.owner_draining_at IS NULL \
                   AND idem.lease_expires_at <> '' \
                   AND idem.lease_expires_at::timestamptz > $5::timestamptz\
             )",
            runs = self.table_name,
            idempotency = self.idempotency_table,
        );
        let owns_fence: bool = sqlx::query_scalar(sqlx::AssertSqlSafe(fence_sql))
            .bind(RUN_OPERATION)
            .bind(&registration.run_id)
            .bind(&registration.flow)
            .bind(self.lease.instance_id())
            .bind(&database_now)
            .bind(&registration.key_hash)
            .bind(&registration.attempt_id)
            .fetch_one(&mut *tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL human-input registration fence lookup failed: {error}"
                ))
            })?;
        if !owns_fence {
            tx.rollback().await.map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL human-input registration rollback failed: {error}"
                ))
            })?;
            return Err(IronCrewError::Conflict(format!(
                "Run '{}' no longer owns the active keyed attempt for this human-input question",
                registration.run_id
            )));
        }

        // Resolve an idempotent retry before applying capacity so retrying an
        // already-retained question never consumes (or appears to consume) a
        // second slot. The per-run resource advisory lock serializes all
        // conforming registrations for exact aggregate admission.
        let existing_sql = format!(
            "SELECT flow, owner_instance_id, key_hash, attempt_id, \
                    question_digest, state, expires_at > $3::timestamptz \
             FROM {} WHERE run_id = $1 AND question_id = $2 FOR UPDATE",
            self.human_inputs_table
        );
        let existing: Option<(String, String, String, String, String, String, bool)> =
            sqlx::query_as(sqlx::AssertSqlSafe(existing_sql))
                .bind(&registration.run_id)
                .bind(&registration.question.question_id)
                .bind(&database_now)
                .fetch_optional(&mut *tx)
                .await
                .map_err(|error| {
                    IronCrewError::Validation(format!(
                        "PostgreSQL human-input idempotent registration lookup failed: {error}"
                    ))
                })?;
        if let Some((flow, owner, key_hash, attempt_id, digest, state, unexpired)) = existing {
            if flow == registration.flow
                && owner == self.lease.instance_id()
                && key_hash == registration.key_hash
                && attempt_id == registration.attempt_id
                && digest == aad.question_digest
                && state == "pending"
                && unexpired
            {
                tx.commit().await.map_err(|error| {
                    IronCrewError::Validation(format!(
                        "PostgreSQL idempotent human-input registration commit failed: {error}"
                    ))
                })?;
                return Ok(HumanInputRegistrationOutcome::Registered);
            }
            return Err(IronCrewError::Conflict(format!(
                "Human-input question '{}' is already registered under another run attempt",
                registration.question.question_id
            )));
        }

        let capacity_sql = format!(
            "SELECT COUNT(*)::BIGINT, \
                    COALESCE(SUM(octet_length(question_nonce) + \
                                 octet_length(question_ciphertext)), 0)::BIGINT \
             FROM {} WHERE run_id = $1 AND flow = $2 AND state = 'pending' \
               AND expires_at > $3::timestamptz",
            self.human_inputs_table
        );
        let (pending_rows, pending_ciphertext_bytes): (i64, i64) =
            sqlx::query_as(sqlx::AssertSqlSafe(capacity_sql))
                .bind(&registration.run_id)
                .bind(&registration.flow)
                .bind(&database_now)
                .fetch_one(&mut *tx)
                .await
                .map_err(|error| {
                    IronCrewError::Validation(format!(
                        "PostgreSQL human-input registration capacity lookup failed: {error}"
                    ))
                })?;
        let pending_rows = usize::try_from(pending_rows).map_err(|_| {
            IronCrewError::Validation("PostgreSQL human-input row accounting is invalid".into())
        })?;
        let pending_ciphertext_bytes = usize::try_from(pending_ciphertext_bytes).map_err(|_| {
            IronCrewError::Validation(
                "PostgreSQL human-input ciphertext accounting is invalid".into(),
            )
        })?;
        let incoming_ciphertext_bytes = encrypted
            .nonce
            .len()
            .checked_add(encrypted.ciphertext.len())
            .ok_or_else(|| {
                IronCrewError::Validation(
                    "PostgreSQL human-input ciphertext byte count overflow".into(),
                )
            })?;
        let projected_ciphertext_bytes = pending_ciphertext_bytes
            .checked_add(incoming_ciphertext_bytes)
            .ok_or_else(|| {
                IronCrewError::Validation(
                    "PostgreSQL human-input ciphertext accounting overflow".into(),
                )
            })?;
        if pending_rows >= self.human_input_max_pending_rows
            || projected_ciphertext_bytes > self.human_input_max_pending_ciphertext_bytes
        {
            return Err(IronCrewError::Conflict(format!(
                "PostgreSQL human-input mailbox reached its configured capacity ({} rows, {} ciphertext bytes)",
                self.human_input_max_pending_rows, self.human_input_max_pending_ciphertext_bytes,
            )));
        }

        let insert_sql = format!(
            "INSERT INTO {} (\
                 run_id, question_id, flow, owner_instance_id, key_hash, attempt_id, \
                 question_digest, question_key_fingerprint, question_nonce, question_ciphertext, \
                 state, created_at, expires_at\
             ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, 'pending', \
                       $11::timestamptz, $12::timestamptz) \
             ON CONFLICT (run_id, question_id) DO NOTHING",
            self.human_inputs_table
        );
        let inserted = sqlx::query(sqlx::AssertSqlSafe(insert_sql))
            .bind(&registration.run_id)
            .bind(&registration.question.question_id)
            .bind(&registration.flow)
            .bind(self.lease.instance_id())
            .bind(&registration.key_hash)
            .bind(&registration.attempt_id)
            .bind(&aad.question_digest)
            .bind(&encrypted.key_fingerprint)
            .bind(&encrypted.nonce)
            .bind(&encrypted.ciphertext)
            .bind(&database_now)
            .bind(&expires_at)
            .execute(&mut *tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL human-input registration insert failed: {error}"
                ))
            })?;
        if inserted.rows_affected() == 0 {
            let existing_sql = format!(
                "SELECT EXISTS (SELECT 1 FROM {} \
                     WHERE run_id = $1 AND question_id = $2 AND flow = $3 \
                       AND owner_instance_id = $4 AND key_hash = $5 AND attempt_id = $6 \
                       AND question_digest = $7 AND state = 'pending' \
                       AND expires_at > $8::timestamptz)",
                self.human_inputs_table
            );
            let same_fence: bool = sqlx::query_scalar(sqlx::AssertSqlSafe(existing_sql))
                .bind(&registration.run_id)
                .bind(&registration.question.question_id)
                .bind(&registration.flow)
                .bind(self.lease.instance_id())
                .bind(&registration.key_hash)
                .bind(&registration.attempt_id)
                .bind(&aad.question_digest)
                .bind(&database_now)
                .fetch_one(&mut *tx)
                .await
                .map_err(|error| {
                    IronCrewError::Validation(format!(
                        "PostgreSQL human-input registration collision lookup failed: {error}"
                    ))
                })?;
            if !same_fence {
                tx.rollback().await.map_err(|error| {
                    IronCrewError::Validation(format!(
                        "PostgreSQL human-input collision rollback failed: {error}"
                    ))
                })?;
                return Err(IronCrewError::Conflict(format!(
                    "Human-input question '{}' is already registered under another run attempt",
                    registration.question.question_id
                )));
            }
        }
        tx.commit().await.map_err(|error| {
            IronCrewError::Validation(format!(
                "PostgreSQL human-input registration commit failed: {error}"
            ))
        })?;
        Ok(HumanInputRegistrationOutcome::Registered)
    }
}
