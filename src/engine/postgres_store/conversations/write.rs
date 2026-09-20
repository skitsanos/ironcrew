use sqlx::Row;

use crate::engine::conversation_record::{
    HARD_STORED_CONVERSATION_EXECUTION_BYTES, serialize_conversation_execution,
    serialize_conversation_messages, validate_conversation_record_for_write,
};
use crate::engine::idempotency::CONVERSATION_MESSAGE_OPERATION;
use crate::engine::sessions::{ConversationRecord, validate_session_id};
use crate::utils::error::{IronCrewError, Result};

use super::super::PostgresStore;
use super::super::codecs::bounded_conversation_execution;

impl PostgresStore {
    pub(in crate::engine::postgres_store) async fn save_conversation_record(
        &self,
        record: &ConversationRecord,
    ) -> Result<u64> {
        validate_conversation_record_for_write(record)?;
        let messages_json = serialize_conversation_messages(&record.messages)?;
        let execution_json = serialize_conversation_execution(&record.execution)?;
        let expected_revision = i64::try_from(record.revision).map_err(|_| {
            IronCrewError::Validation("Conversation revision is out of range".into())
        })?;
        let mut tx = self.pool.begin().await.map_err(|e| {
            IronCrewError::Validation(format!(
                "PostgreSQL save_conversation transaction error: {e}"
            ))
        })?;
        self.lock_resource(
            &mut tx,
            CONVERSATION_MESSAGE_OPERATION,
            record.flow_path.as_deref().unwrap_or(""),
            &record.id,
        )
        .await?;
        let guard_sql = format!(
            "SELECT EXISTS (SELECT 1 FROM {} \
             WHERE operation = $1 AND scope = $2 AND resource_id = $3 \
               AND state IN ('claimed', 'running'))",
            self.idempotency_table
        );
        let active: bool = sqlx::query_scalar(sqlx::AssertSqlSafe(guard_sql))
            .bind(CONVERSATION_MESSAGE_OPERATION)
            .bind(record.flow_path.as_deref().unwrap_or(""))
            .bind(&record.id)
            .fetch_one(&mut *tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL conversation idempotency guard failed: {error}"
                ))
            })?;
        if active {
            return Err(IronCrewError::Conflict(format!(
                "Conversation '{}' has an active idempotent message operation",
                record.id
            )));
        }
        let select_sql = format!(
            "SELECT revision, \
                    CASE WHEN octet_length(execution::text) <= $3 THEN execution::text END AS execution, \
                    octet_length(execution::text)::BIGINT AS execution_bytes FROM {} \
             WHERE id = $1 AND flow_path IS NOT DISTINCT FROM $2 FOR UPDATE",
            self.conversations_table
        );
        let current = sqlx::query(sqlx::AssertSqlSafe(select_sql))
            .bind(&record.id)
            .bind(&record.flow_path)
            .bind(i64::try_from(HARD_STORED_CONVERSATION_EXECUTION_BYTES).unwrap_or(i64::MAX))
            .fetch_optional(&mut *tx)
            .await
            .map_err(|e| {
                IronCrewError::Validation(format!(
                    "PostgreSQL save_conversation revision read error: {e}"
                ))
            })?;
        let current = current
            .map(|row| {
                let revision = row.try_get::<i64, _>("revision").map_err(|error| {
                    IronCrewError::Validation(format!(
                        "PostgreSQL conversation revision decode failed: {error}"
                    ))
                })?;
                let execution = bounded_conversation_execution(&row)?;
                Ok::<_, IronCrewError>((revision, execution))
            })
            .transpose()?;
        let revision: Option<i64> = match current {
            None if expected_revision == 0 => {
                let insert_sql = format!(
                    "INSERT INTO {} \
                     (id, flow_name, flow_path, agent_name, execution, messages, created_at, updated_at, revision) \
                     VALUES ($1, $2, $3, $4, $5::jsonb, $6::jsonb, $7, $8, 1) \
                     ON CONFLICT (flow_path, id) DO NOTHING RETURNING revision",
                    self.conversations_table
                );
                sqlx::query_scalar(sqlx::AssertSqlSafe(insert_sql))
                    .bind(&record.id)
                    .bind(&record.flow_name)
                    .bind(&record.flow_path)
                    .bind(&record.agent_name)
                    .bind(&execution_json)
                    .bind(&messages_json)
                    .bind(&record.created_at)
                    .bind(&record.updated_at)
                    .fetch_optional(&mut *tx)
                    .await
                    .map_err(|e| {
                        IronCrewError::Validation(format!(
                            "PostgreSQL save_conversation insert error: {e}"
                        ))
                    })?
            }
            Some((current_revision, current_execution))
                if current_revision == expected_revision
                    && current_execution == record.execution =>
            {
                let update_sql = format!(
                    "UPDATE {} SET flow_name = $3, agent_name = $4, \
                     execution = $5::jsonb, messages = $6::jsonb, created_at = $7, updated_at = $8, \
                     revision = revision + 1 \
                     WHERE id = $1 AND flow_path IS NOT DISTINCT FROM $2 AND revision = $9 \
                     RETURNING revision",
                    self.conversations_table
                );
                sqlx::query_scalar(sqlx::AssertSqlSafe(update_sql))
                    .bind(&record.id)
                    .bind(&record.flow_path)
                    .bind(&record.flow_name)
                    .bind(&record.agent_name)
                    .bind(&execution_json)
                    .bind(&messages_json)
                    .bind(&record.created_at)
                    .bind(&record.updated_at)
                    .bind(expected_revision)
                    .fetch_optional(&mut *tx)
                    .await
                    .map_err(|e| {
                        IronCrewError::Validation(format!(
                            "PostgreSQL save_conversation update error: {e}"
                        ))
                    })?
            }
            _ => None,
        };
        let revision = revision.ok_or_else(|| {
            IronCrewError::Conflict(format!(
                "Conversation '{}' changed since revision {}; reopen it before saving",
                record.id, record.revision
            ))
        })?;
        tx.commit().await.map_err(|e| {
            IronCrewError::Validation(format!("PostgreSQL save_conversation commit error: {e}"))
        })?;
        u64::try_from(revision)
            .map_err(|_| IronCrewError::Validation("Invalid conversation revision".into()))
    }

    pub(in crate::engine::postgres_store) async fn delete_conversation_record(
        &self,
        flow_path: Option<&str>,
        id: &str,
    ) -> Result<()> {
        validate_session_id(id)?;
        let mut tx = self.pool.begin().await.map_err(|error| {
            IronCrewError::Validation(format!(
                "PostgreSQL delete_conversation transaction error: {error}"
            ))
        })?;
        self.lock_resource(
            &mut tx,
            CONVERSATION_MESSAGE_OPERATION,
            flow_path.unwrap_or(""),
            id,
        )
        .await?;
        let guard_sql = format!(
            "SELECT EXISTS (SELECT 1 FROM {} \
             WHERE operation = $1 AND scope = $2 AND resource_id = $3 \
               AND state IN ('claimed', 'running'))",
            self.idempotency_table
        );
        let active: bool = sqlx::query_scalar(sqlx::AssertSqlSafe(guard_sql))
            .bind(CONVERSATION_MESSAGE_OPERATION)
            .bind(flow_path.unwrap_or(""))
            .bind(id)
            .fetch_one(&mut *tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL conversation delete idempotency guard failed: {error}"
                ))
            })?;
        if active {
            return Err(IronCrewError::Conflict(format!(
                "Conversation '{id}' has an active idempotent message operation"
            )));
        }
        let sql = format!(
            "DELETE FROM {} WHERE id = $1 AND ($2::TEXT IS NULL OR flow_path = $2)",
            self.conversations_table
        );
        sqlx::query(sqlx::AssertSqlSafe(sql.to_string()))
            .bind(id)
            .bind(flow_path)
            .execute(&mut *tx)
            .await
            .map_err(|e| {
                IronCrewError::Validation(format!("PostgreSQL delete_conversation error: {}", e))
            })?;
        tx.commit().await.map_err(|error| {
            IronCrewError::Validation(format!(
                "PostgreSQL delete_conversation commit error: {error}"
            ))
        })?;
        Ok(())
    }
}
