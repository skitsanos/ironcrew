use std::time::Duration;

use crate::utils::error::{IronCrewError, Result};

use super::super::{PostgresStore, RUN_RECONCILIATION_BATCH_SIZE};

impl PostgresStore {
    pub(in crate::engine::postgres_store) async fn delete_human_inputs_for_run(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        run_id: &str,
    ) -> Result<u64> {
        let sql = format!("DELETE FROM {} WHERE run_id = $1", self.human_inputs_table);
        let deleted = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(run_id)
            .execute(&mut **tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL human-input mailbox cleanup failed: {error}"
                ))
            })?;
        Ok(deleted.rows_affected())
    }

    pub(in crate::engine::postgres_store) async fn delete_expired_human_input_batch(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        database_now: &str,
    ) -> Result<()> {
        let sql = format!(
            "WITH candidates AS (\
                 SELECT human.run_id, human.question_id \
                 FROM {human_inputs} AS human \
                 WHERE (human.state = 'pending' AND \
                        human.expires_at <= $1::timestamptz) \
                    OR EXISTS (\
                        SELECT 1 FROM {runs} AS run \
                        WHERE run.run_id = human.run_id \
                          AND run.status NOT IN ('running', 'waiting_for_input')\
                    ) \
                 ORDER BY human.expires_at, human.run_id, human.question_id \
                 LIMIT $2 FOR UPDATE SKIP LOCKED\
             ) \
             DELETE FROM {human_inputs} AS human \
             USING candidates \
             WHERE human.run_id = candidates.run_id \
               AND human.question_id = candidates.question_id",
            human_inputs = self.human_inputs_table,
            runs = self.table_name,
        );
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(database_now)
            .bind(RUN_RECONCILIATION_BATCH_SIZE)
            .execute(&mut **tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL expired human-input mailbox cleanup failed: {error}"
                ))
            })?;
        Ok(())
    }

    /// Refuse a process configuration that cannot decrypt ciphertext already
    /// retained in the shared mailbox.
    ///
    /// The keyring is immutable for the lifetime of a process, so this is a
    /// startup gate rather than a recurring readiness-table scan. Runtime
    /// mailbox operations still authenticate each row before mutation, which
    /// fails closed if a stale or rogue old-active revision writes after this
    /// snapshot. Operators must stop every old-active writer and drain all old
    /// fingerprint rows before deploying a keyring that removes the old key.
    pub(in crate::engine::postgres_store) async fn verify_human_input_key_coverage(
        &self,
        timeout: Duration,
    ) -> Result<()> {
        let outer_timeout = timeout
            .checked_add(Duration::from_secs(1))
            .unwrap_or(timeout);
        tokio::time::timeout(
            outer_timeout,
            self.verify_human_input_key_coverage_inner(timeout),
        )
        .await
        .map_err(|_| {
            IronCrewError::Validation(
                "PostgreSQL human-input key coverage startup check timed out".into(),
            )
        })?
    }

    async fn verify_human_input_key_coverage_inner(&self, timeout: Duration) -> Result<()> {
        let configured_fingerprints = self
            .human_input_keyring
            .as_ref()
            .map(|keyring| {
                keyring
                    .fingerprints()
                    .map(str::to_owned)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let timeout_value = format!("{}ms", timeout.as_millis().max(1));
        let mut tx = self.pool.begin().await.map_err(|error| {
            IronCrewError::Validation(format!(
                "Failed to begin PostgreSQL human-input key coverage check: {error}"
            ))
        })?;
        sqlx::query(
            "SELECT set_config('lock_timeout', $1, true), \
                    set_config('statement_timeout', $1, true)",
        )
        .bind(&timeout_value)
        .execute(&mut *tx)
        .await
        .map_err(|error| {
            IronCrewError::Validation(format!(
                "Failed to bound PostgreSQL human-input key coverage check: {error}"
            ))
        })?;
        let sql = format!(
            "SELECT EXISTS (\
                 SELECT 1 FROM (\
                     SELECT question_key_fingerprint AS fingerprint FROM {human_inputs} \
                     UNION ALL \
                     SELECT answer_key_fingerprint AS fingerprint FROM {human_inputs} \
                     WHERE answer_key_fingerprint IS NOT NULL\
                 ) AS encrypted \
                 WHERE NOT (fingerprint = ANY($1::text[]))\
             )",
            human_inputs = self.human_inputs_table,
        );
        let unsupported: bool = sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
            .bind(configured_fingerprints)
            .fetch_one(&mut *tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "Failed to verify PostgreSQL human-input key coverage: {error}"
                ))
            })?;
        if unsupported {
            return Err(IronCrewError::Validation(
                "PostgreSQL human-input mailbox contains ciphertext for an unavailable encryption key; restore the complete keyring and drain every retiring-key row before restarting"
                    .into(),
            ));
        }
        tx.commit().await.map_err(|error| {
            IronCrewError::Validation(format!(
                "Failed to commit PostgreSQL human-input key coverage check: {error}"
            ))
        })?;
        Ok(())
    }
}
