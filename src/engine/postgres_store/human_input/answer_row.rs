use sqlx::Row;

use crate::utils::error::{IronCrewError, Result};

use super::super::PostgresStore;

pub(super) struct HumanInputAnswerRow {
    pub(super) owner_instance_id: String,
    pub(super) key_hash: String,
    pub(super) attempt_id: String,
    pub(super) question_digest: String,
    pub(super) question_key_fingerprint: String,
    pub(super) question_nonce: Vec<u8>,
    pub(super) question_ciphertext: Vec<u8>,
    pub(super) state: String,
}

impl PostgresStore {
    pub(super) async fn load_bounded_human_input_answer_row(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        flow: &str,
        run_id: &str,
        question_id: &str,
    ) -> Result<Option<HumanInputAnswerRow>> {
        // Lock and inspect fixed-size metadata before transferring any stored
        // text or BYTEA value into the pod. The mailbox can be written by an
        // external database client, so schema constraints and registration
        // admission are not sufficient pre-materialization defenses.
        let bounds_sql = format!(
            "SELECT octet_length(owner_instance_id)::BIGINT, \
                    octet_length(key_hash)::BIGINT, octet_length(attempt_id)::BIGINT, \
                    octet_length(question_digest)::BIGINT, \
                    octet_length(question_key_fingerprint)::BIGINT, \
                    octet_length(question_nonce)::BIGINT, \
                    octet_length(question_ciphertext)::BIGINT \
             FROM {} WHERE run_id = $1 AND question_id = $2 AND flow = $3 \
             FOR UPDATE",
            self.human_inputs_table
        );
        let bounds: Option<(i64, i64, i64, i64, i64, i64, i64)> =
            sqlx::query_as(sqlx::AssertSqlSafe(bounds_sql))
                .bind(run_id)
                .bind(question_id)
                .bind(flow)
                .fetch_optional(&mut **tx)
                .await
                .map_err(|error| {
                    IronCrewError::Validation(format!(
                        "PostgreSQL human-input answer lookup failed: {error}"
                    ))
                })?;
        let Some((
            owner_bytes,
            key_hash_bytes,
            attempt_bytes,
            digest_bytes,
            fingerprint_bytes,
            nonce_bytes,
            ciphertext_bytes,
        )) = bounds
        else {
            return Ok(None);
        };
        let max_ciphertext_bytes = i64::try_from(self.human_input_max_pending_ciphertext_bytes)
            .map_err(|_| {
                IronCrewError::Validation(
                    "PostgreSQL human-input ciphertext bound is invalid".into(),
                )
            })?;
        let encrypted_bytes = nonce_bytes.checked_add(ciphertext_bytes);
        if !(1..=255).contains(&owner_bytes)
            || key_hash_bytes != 64
            || !(1..=128).contains(&attempt_bytes)
            || digest_bytes != 64
            || fingerprint_bytes != 64
            || nonce_bytes != 12
            || ciphertext_bytes <= 16
            || encrypted_bytes.is_none_or(|bytes| bytes > max_ciphertext_bytes)
        {
            return Err(IronCrewError::Validation(
                "PostgreSQL human-input question metadata exceeds its authenticated bounds".into(),
            ));
        }

        let row_sql = format!(
            "SELECT owner_instance_id, key_hash, attempt_id, question_digest, \
                    question_key_fingerprint, question_nonce, question_ciphertext, state \
             FROM {} WHERE run_id = $1 AND question_id = $2 AND flow = $3",
            self.human_inputs_table
        );
        let row = sqlx::query(sqlx::AssertSqlSafe(row_sql))
            .bind(run_id)
            .bind(question_id)
            .bind(flow)
            .fetch_one(&mut **tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL bounded human-input answer lookup failed: {error}"
                ))
            })?;
        Ok(Some(HumanInputAnswerRow {
            owner_instance_id: row
                .try_get("owner_instance_id")
                .map_err(|error| IronCrewError::Validation(format!("Column error: {error}")))?,
            key_hash: row
                .try_get("key_hash")
                .map_err(|error| IronCrewError::Validation(format!("Column error: {error}")))?,
            attempt_id: row
                .try_get("attempt_id")
                .map_err(|error| IronCrewError::Validation(format!("Column error: {error}")))?,
            question_digest: row
                .try_get("question_digest")
                .map_err(|error| IronCrewError::Validation(format!("Column error: {error}")))?,
            question_key_fingerprint: row
                .try_get("question_key_fingerprint")
                .map_err(|error| IronCrewError::Validation(format!("Column error: {error}")))?,
            question_nonce: row
                .try_get("question_nonce")
                .map_err(|error| IronCrewError::Validation(format!("Column error: {error}")))?,
            question_ciphertext: row
                .try_get("question_ciphertext")
                .map_err(|error| IronCrewError::Validation(format!("Column error: {error}")))?,
            state: row
                .try_get("state")
                .map_err(|error| IronCrewError::Validation(format!("Column error: {error}")))?,
        }))
    }
}
