use sqlx::Row;

use crate::engine::conversation_record::HARD_STORED_CONVERSATION_EXECUTION_BYTES;
use crate::engine::idempotency::{CONVERSATION_MESSAGE_OPERATION, IdempotencyClaim};
use crate::engine::sessions::{conversation_mutation_scope, validate_session_id};
use crate::utils::error::{IronCrewError, Result};

use super::super::super::PostgresStore;
use super::super::super::codecs::bounded_conversation_execution;

impl PostgresStore {
    pub(super) async fn conversation_claim_is_current(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        claim: &IdempotencyClaim,
    ) -> Result<bool> {
        if claim.operation != CONVERSATION_MESSAGE_OPERATION {
            return Ok(true);
        }
        validate_session_id(&claim.resource_id)?;
        let expected_revision = claim.base_revision.ok_or_else(|| {
            IronCrewError::Validation("Conversation idempotency claim has no base revision".into())
        })?;
        let sql = format!(
            "SELECT revision, \
                    CASE WHEN octet_length(execution::text) <= $3 THEN execution::text END AS execution, \
                    octet_length(execution::text)::BIGINT AS execution_bytes FROM {} \
             WHERE id = $1 AND flow_path IS NOT DISTINCT FROM $2 FOR UPDATE",
            self.conversations_table
        );
        let current = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(&claim.resource_id)
            .bind(&claim.scope)
            .bind(i64::try_from(HARD_STORED_CONVERSATION_EXECUTION_BYTES).unwrap_or(i64::MAX))
            .fetch_optional(&mut **tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL conversation idempotency revision query failed: {error}"
                ))
            })?;
        current
            .map(|row| {
                let revision =
                    u64::try_from(row.try_get::<i64, _>("revision").map_err(|error| {
                        IronCrewError::Validation(format!(
                            "PostgreSQL conversation revision decode failed: {error}"
                        ))
                    })?)
                    .map_err(|_| {
                        IronCrewError::Validation(
                            "PostgreSQL conversation revision is negative".into(),
                        )
                    })?;
                let execution = bounded_conversation_execution(&row)?;
                let expected_scope = conversation_mutation_scope(
                    &claim.scope,
                    &claim.resource_id,
                    &execution.incarnation_id,
                );
                Ok(execution.validate().is_ok()
                    && revision == expected_revision
                    && claim.exclusive_scope.as_deref() == Some(expected_scope.as_str()))
            })
            .transpose()
            .map(Option::unwrap_or_default)
    }
}
