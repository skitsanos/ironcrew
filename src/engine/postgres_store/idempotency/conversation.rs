use sqlx::Row;

use crate::engine::conversation_record::{
    HARD_STORED_CONVERSATION_EXECUTION_BYTES, serialize_conversation_execution,
    serialize_conversation_messages, validate_conversation_record_for_write,
};
use crate::engine::idempotency::{
    CONVERSATION_MESSAGE_OPERATION, ConversationIdempotencyCommit, IdempotencyCompletion,
    IdempotencyLimits, IdempotencyState,
};
use crate::engine::sessions::{ConversationRecord, conversation_mutation_scope};
use crate::utils::error::{IronCrewError, Result};

use super::super::PostgresStore;
use super::super::codecs::bounded_conversation_execution;
impl PostgresStore {
    pub(in crate::engine::postgres_store) async fn commit_conversation_idempotency_record(
        &self,
        completion: IdempotencyCompletion,
        conversation: &ConversationRecord,
        limits: IdempotencyLimits,
    ) -> Result<ConversationIdempotencyCommit> {
        completion.validate()?;
        limits.validate()?;
        validate_conversation_record_for_write(conversation)?;
        let messages_json = serialize_conversation_messages(&conversation.messages)?;
        let execution_json = serialize_conversation_execution(&conversation.execution)?;
        let usage_json = crate::engine::session_usage::encode(&conversation.usage)?;
        let expected_revision = i64::try_from(conversation.revision).map_err(|_| {
            IronCrewError::Validation("Conversation revision is out of range".into())
        })?;
        let mut tx = self.pool.begin().await.map_err(|error| {
            IronCrewError::Validation(format!(
                "PostgreSQL idempotent conversation transaction failed: {error}"
            ))
        })?;
        self.lock_idempotency_quota(&mut tx).await?;
        self.lock_idempotency_principal(&mut tx, &completion.principal_id)
            .await?;
        self.lock_resource(
            &mut tx,
            CONVERSATION_MESSAGE_OPERATION,
            conversation.flow_path.as_deref().unwrap_or(""),
            &conversation.id,
        )
        .await?;
        self.lock_idempotency_key(&mut tx, &completion.key_hash)
            .await?;
        let record = self
            .get_idempotency_in_transaction(&mut tx, &completion.key_hash)
            .await?
            .ok_or_else(|| {
                IronCrewError::Validation(
                    "Idempotency claim not found during conversation commit".into(),
                )
            })?;
        if record.principal_id != completion.principal_id
            || record.request_fingerprint != completion.request_fingerprint
            || record.attempt_id != completion.attempt_id
            || record.owner_instance_id != completion.owner_instance_id
        {
            return Err(IronCrewError::Conflict(format!(
                "Idempotency claim '{}' is fenced by a different attempt",
                completion.key_hash
            )));
        }
        let expected_scope = conversation_mutation_scope(
            conversation.flow_path.as_deref().unwrap_or(""),
            &conversation.id,
            &conversation.execution.incarnation_id,
        );
        if record.operation != CONVERSATION_MESSAGE_OPERATION
            || record.resource_id != conversation.id
            || record.scope != conversation.flow_path.as_deref().unwrap_or("")
            || record.exclusive_scope.as_deref() != Some(expected_scope.as_str())
        {
            return Err(IronCrewError::Conflict(format!(
                "Idempotency claim '{}' does not match the conversation scope",
                completion.key_hash
            )));
        }
        let base_revision = record.base_revision.ok_or_else(|| {
            IronCrewError::Validation("Conversation idempotency claim has no base revision".into())
        })?;
        if base_revision != conversation.revision {
            return Err(IronCrewError::Conflict(format!(
                "Conversation '{}' changed before idempotent commit",
                conversation.id
            )));
        }
        if record.state == IdempotencyState::Completed {
            tx.commit().await.map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL completed conversation commit failed: {error}"
                ))
            })?;
            return Ok(ConversationIdempotencyCommit {
                revision: base_revision.saturating_add(1),
                replayable: record.replayable(),
                already_completed: true,
            });
        }
        if record.state == IdempotencyState::Indeterminate {
            return Err(IronCrewError::Conflict(
                "Indeterminate conversation outcomes cannot be committed".into(),
            ));
        }
        let (database_completed_at, database_expires_at) = self
            .database_clock_with_deadline(
                &mut tx,
                record.ttl_seconds,
                "conversation idempotency completion",
            )
            .await?;

        let select_sql = format!(
            "SELECT revision, \
                    CASE WHEN octet_length(execution::text) <= $3 THEN execution::text END AS execution, \
                    octet_length(execution::text)::BIGINT AS execution_bytes FROM {} \
             WHERE id = $1 AND flow_path IS NOT DISTINCT FROM $2 FOR UPDATE",
            self.conversations_table
        );
        let current = sqlx::query(sqlx::AssertSqlSafe(select_sql))
            .bind(&conversation.id)
            .bind(&conversation.flow_path)
            .bind(i64::try_from(HARD_STORED_CONVERSATION_EXECUTION_BYTES).unwrap_or(i64::MAX))
            .fetch_optional(&mut *tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL idempotent conversation revision read failed: {error}"
                ))
            })?;
        let current = current
            .map(|row| {
                let revision = row.try_get::<i64, _>("revision").map_err(|error| {
                    IronCrewError::Validation(format!(
                        "PostgreSQL conversation revision decode failed: {error}"
                    ))
                })?;
                let stored_execution = bounded_conversation_execution(&row)?;
                Ok::<_, IronCrewError>((revision, stored_execution))
            })
            .transpose()?;
        let revision: Option<i64> = match current {
            Some((current_revision, current_execution))
                if current_revision == expected_revision
                    && current_execution == conversation.execution =>
            {
                let update_sql = format!(
                    "UPDATE {} SET flow_name = $3, agent_name = $4, \
                     execution = $5::jsonb, messages = $6::jsonb, created_at = $7, updated_at = $8, \
                     revision = revision + 1, usage = $10::jsonb \
                     WHERE id = $1 AND flow_path IS NOT DISTINCT FROM $2 AND revision = $9 \
                     RETURNING revision",
                    self.conversations_table
                );
                sqlx::query_scalar(sqlx::AssertSqlSafe(update_sql))
                    .bind(&conversation.id)
                    .bind(&conversation.flow_path)
                    .bind(&conversation.flow_name)
                    .bind(&conversation.agent_name)
                    .bind(&execution_json)
                    .bind(&messages_json)
                    .bind(&conversation.created_at)
                    .bind(&conversation.updated_at)
                    .bind(expected_revision)
                    .bind(&usage_json)
                    .fetch_optional(&mut *tx)
                    .await
                    .map_err(|error| {
                        IronCrewError::Validation(format!(
                            "PostgreSQL idempotent conversation update failed: {error}"
                        ))
                    })?
            }
            _ => None,
        };
        let revision = revision.ok_or_else(|| {
            IronCrewError::Conflict(format!(
                "Conversation '{}' changed since revision {}; reopen it before saving",
                conversation.id, conversation.revision
            ))
        })?;

        let (global_usage, principal_usage) = self
            .idempotency_accounting_for_update(&mut tx, &completion.principal_id)
            .await?;
        let old_response_bytes = record.response_body.as_ref().map_or(0, String::len);
        let global_without_record = global_usage
            .response_bytes
            .checked_sub(old_response_bytes)
            .ok_or_else(|| {
                IronCrewError::Validation(
                    "PostgreSQL global idempotency response accounting is inconsistent".into(),
                )
            })?;
        let principal_without_record = principal_usage
            .response_bytes
            .checked_sub(old_response_bytes)
            .ok_or_else(|| {
                IronCrewError::Validation(
                    "PostgreSQL principal idempotency response accounting is inconsistent".into(),
                )
            })?;
        let response_body = completion.response_body.as_ref().filter(|body| {
            global_without_record
                .checked_add(body.len())
                .is_some_and(|total| total <= limits.global_max_response_bytes)
                && principal_without_record
                    .checked_add(body.len())
                    .is_some_and(|total| total <= limits.principal_max_response_bytes)
        });
        let update_idempotency = format!(
            "UPDATE {} SET state = 'completed', response_status = $1, \
             response_body = $2, lease_expires_at = '', updated_at = $3, \
             completed_at = $3, expires_at = $4 \
             WHERE key_hash = $5 AND request_fingerprint = $6 \
               AND attempt_id = $7 AND owner_instance_id = $8 \
               AND state IN ('claimed', 'running')",
            self.idempotency_table
        );
        let updated = sqlx::query(sqlx::AssertSqlSafe(update_idempotency))
            .bind(i32::from(completion.response_status))
            .bind(response_body)
            .bind(&database_completed_at)
            .bind(&database_expires_at)
            .bind(&completion.key_hash)
            .bind(&completion.request_fingerprint)
            .bind(&completion.attempt_id)
            .bind(&completion.owner_instance_id)
            .execute(&mut *tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL idempotent conversation completion failed: {error}"
                ))
            })?;
        if updated.rows_affected() != 1 {
            return Err(IronCrewError::Conflict(format!(
                "Idempotency claim '{}' changed before conversation commit",
                completion.key_hash
            )));
        }
        tx.commit().await.map_err(|error| {
            IronCrewError::Validation(format!(
                "PostgreSQL idempotent conversation commit failed: {error}"
            ))
        })?;
        Ok(ConversationIdempotencyCommit {
            revision: u64::try_from(revision)
                .map_err(|_| IronCrewError::Validation("Invalid conversation revision".into()))?,
            replayable: response_body.is_some(),
            already_completed: false,
        })
    }
}
