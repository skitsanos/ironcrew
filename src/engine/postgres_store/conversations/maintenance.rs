use crate::engine::idempotency::CONVERSATION_MESSAGE_OPERATION;
use crate::utils::error::{IronCrewError, Result};

use super::super::{PostgresStore, RUN_RECONCILIATION_BATCH_SIZE};

impl PostgresStore {
    pub(in crate::engine::postgres_store) async fn reconcile_expired_conversation_batch(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        database_now: &str,
    ) -> Result<()> {
        let sql = format!(
            "WITH candidates AS (\
                 SELECT key_hash FROM {idempotency} \
                 WHERE operation = $2 AND state IN ('claimed', 'running') \
                   AND lease_expires_at::timestamptz <= $1::timestamptz \
                 ORDER BY lease_expires_at, key_hash \
                 LIMIT $3 FOR UPDATE SKIP LOCKED\
             ) \
             UPDATE {idempotency} AS idem \
             SET state = 'indeterminate', response_status = NULL, \
                 response_body = NULL, lease_expires_at = '', updated_at = $1, \
                 completed_at = $1, expires_at = to_char(\
                     ($1::timestamptz + idem.ttl_seconds * interval '1 second') \
                         AT TIME ZONE 'UTC', \
                     'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"'\
                 ) \
             FROM candidates \
             WHERE idem.key_hash = candidates.key_hash",
            idempotency = self.idempotency_table,
        );
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(database_now)
            .bind(CONVERSATION_MESSAGE_OPERATION)
            .bind(RUN_RECONCILIATION_BATCH_SIZE)
            .execute(&mut **tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PG conversation idempotency reconciliation: {error}"
                ))
            })?;
        Ok(())
    }
}
