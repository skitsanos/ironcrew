use super::super::*;

impl PostgresStore {
    pub(super) async fn bootstrap_human_input(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    ) -> Result<()> {
        let t = &self.table_name;
        // 8. Durable human-input mailbox. Question metadata and answers are
        // application-encrypted before they enter SQL; only routing/fencing
        // fields remain queryable. The run FK is both a safety net and the
        // final cleanup path for explicit run deletion.
        let hit = &self.human_inputs_table;
        let human_inputs_sql = format!(
            "CREATE TABLE IF NOT EXISTS {hit} (
                run_id                    TEXT NOT NULL,
                question_id               TEXT NOT NULL,
                flow                      TEXT NOT NULL,
                owner_instance_id         TEXT NOT NULL,
                key_hash                  TEXT NOT NULL,
                attempt_id                TEXT NOT NULL,
                question_digest           TEXT NOT NULL,
                question_key_fingerprint  TEXT NOT NULL,
                question_nonce            BYTEA NOT NULL,
                question_ciphertext       BYTEA NOT NULL,
                answer_key_fingerprint    TEXT,
                answer_nonce              BYTEA,
                answer_ciphertext         BYTEA,
                state                     TEXT NOT NULL DEFAULT 'pending',
                created_at                TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
                expires_at                TIMESTAMPTZ NOT NULL,
                answered_at               TIMESTAMPTZ,
                PRIMARY KEY (run_id, question_id),
                CONSTRAINT {hit}_run_fk FOREIGN KEY (run_id)
                    REFERENCES {t} (run_id) ON DELETE CASCADE,
                CONSTRAINT {hit}_state_ck CHECK (state IN ('pending', 'answered')),
                CONSTRAINT {hit}_payload_ck CHECK (
                    octet_length(question_nonce) > 0 AND
                    octet_length(question_ciphertext) > 0 AND
                    length(question_key_fingerprint) > 0 AND
                    length(question_digest) = 64 AND
                    question_digest !~ '[^0-9a-f]' AND
                    ((state = 'pending' AND answer_key_fingerprint IS NULL AND
                      answer_nonce IS NULL AND answer_ciphertext IS NULL AND
                      answered_at IS NULL) OR
                     (state = 'answered' AND answer_key_fingerprint IS NOT NULL AND
                      answer_nonce IS NOT NULL AND answer_ciphertext IS NOT NULL AND
                      answered_at IS NOT NULL))
                ),
                CONSTRAINT {hit}_expiry_ck CHECK (expires_at > created_at)
            )"
        );
        sqlx::query(sqlx::AssertSqlSafe(human_inputs_sql))
            .execute(&mut **tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "Failed to create PostgreSQL human-input mailbox table '{hit}': {error}"
                ))
            })?;
        // Question AAD gained a semantic digest after the first mailbox
        // rollout. Old rows cannot be authenticated under the new AAD and are
        // intentionally discarded instead of being silently reinterpreted.
        let add_question_digest =
            format!("ALTER TABLE {hit} ADD COLUMN IF NOT EXISTS question_digest TEXT");
        sqlx::query(sqlx::AssertSqlSafe(add_question_digest))
            .execute(&mut **tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "Failed to add PostgreSQL human-input question digest: {error}"
                ))
            })?;
        let discard_legacy_questions = format!("DELETE FROM {hit} WHERE question_digest IS NULL");
        sqlx::query(sqlx::AssertSqlSafe(discard_legacy_questions))
            .execute(&mut **tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "Failed to discard legacy PostgreSQL human-input rows: {error}"
                ))
            })?;
        let require_question_digest =
            format!("ALTER TABLE {hit} ALTER COLUMN question_digest SET NOT NULL");
        sqlx::query(sqlx::AssertSqlSafe(require_question_digest))
            .execute(&mut **tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "Failed to require PostgreSQL human-input question digest: {error}"
                ))
            })?;
        let refresh_human_payload_constraint = format!(
            "ALTER TABLE {hit} DROP CONSTRAINT IF EXISTS {hit}_payload_ck; \
             ALTER TABLE {hit} ADD CONSTRAINT {hit}_payload_ck CHECK (\
                 octet_length(question_nonce) > 0 AND \
                 octet_length(question_ciphertext) > 0 AND \
                 length(question_key_fingerprint) > 0 AND \
                 length(question_digest) = 64 AND \
                 question_digest !~ '[^0-9a-f]' AND \
                 ((state = 'pending' AND answer_key_fingerprint IS NULL AND \
                   answer_nonce IS NULL AND answer_ciphertext IS NULL AND \
                   answered_at IS NULL) OR \
                  (state = 'answered' AND answer_key_fingerprint IS NOT NULL AND \
                   answer_nonce IS NOT NULL AND answer_ciphertext IS NOT NULL AND \
                   answered_at IS NOT NULL))\
             )"
        );
        sqlx::raw_sql(sqlx::AssertSqlSafe(refresh_human_payload_constraint))
            .execute(&mut **tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "Failed to refresh PostgreSQL human-input payload constraint: {error}"
                ))
            })?;
        for sql in [
            format!(
                "CREATE INDEX IF NOT EXISTS {hit}_run_idx ON {hit} (run_id, expires_at) \
                 WHERE state = 'pending'"
            ),
            format!("CREATE INDEX IF NOT EXISTS {hit}_exp_idx ON {hit} (expires_at)"),
            format!(
                "CREATE INDEX IF NOT EXISTS {hit}_pex_idx \
                 ON {hit} (expires_at, run_id, question_id) \
                 WHERE state = 'pending'"
            ),
        ] {
            sqlx::query(sqlx::AssertSqlSafe(sql))
                .execute(&mut **tx)
                .await
                .map_err(|error| {
                    IronCrewError::Validation(format!(
                        "Failed to create PostgreSQL human-input mailbox index: {error}"
                    ))
                })?;
        }
        Ok(())
    }
}
