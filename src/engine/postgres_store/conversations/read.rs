use sqlx::Row;

use crate::engine::conversation_json::{
    preflight_conversation_execution_json, preflight_conversation_messages_json,
};
use crate::engine::conversation_record::{
    HARD_STORED_CONVERSATION_EXECUTION_BYTES, HARD_STORED_CONVERSATION_MESSAGES,
    HARD_STORED_CONVERSATION_MESSAGES_BYTES, HARD_STORED_CONVERSATION_METADATA_BYTES,
    validate_conversation_record_after_decode, validate_stored_conversation_envelope,
};
use crate::engine::sessions::{ConversationRecord, validate_session_id};
use crate::utils::error::{IronCrewError, Result};

use super::super::PostgresStore;
use super::super::codecs::{
    bounded_metadata, bounded_optional_metadata, decode_stored_json, stored_bytes,
};

impl PostgresStore {
    pub(in crate::engine::postgres_store) async fn get_conversation_record(
        &self,
        flow_path: Option<&str>,
        id: &str,
    ) -> Result<Option<ConversationRecord>> {
        validate_session_id(id)?;
        // Flow-scoped lookup: when `flow_path` is Some, require an exact
        // match. `$2::TEXT IS NULL` lets the same query serve global
        // (unscoped) admin lookups.
        let sql = format!(
            "SELECT id, \
                    CASE WHEN octet_length(flow_name) <= $3 THEN flow_name END AS flow_name, \
                    octet_length(flow_name)::BIGINT AS flow_name_bytes, \
                    CASE WHEN flow_path IS NULL OR octet_length(flow_path) <= $3 THEN flow_path END AS flow_path, \
                    octet_length(flow_path)::BIGINT AS flow_path_bytes, \
                    CASE WHEN octet_length(agent_name) <= $3 THEN agent_name END AS agent_name, \
                    octet_length(agent_name)::BIGINT AS agent_name_bytes, \
                    CASE WHEN octet_length(execution::text) <= $4 THEN execution::text END AS execution, \
                    octet_length(execution::text)::BIGINT AS execution_bytes, \
                    CASE \
                      WHEN octet_length(messages::text) <= $5 \
                       AND CASE WHEN jsonb_typeof(messages) = 'array' \
                                THEN jsonb_array_length(messages)::BIGINT <= $6 \
                                ELSE FALSE END \
                      THEN messages::text \
                    END AS messages, \
                    octet_length(messages::text)::BIGINT AS messages_bytes, \
                    CASE WHEN jsonb_typeof(messages) = 'array' \
                         THEN jsonb_array_length(messages)::BIGINT END AS message_count, \
                    CASE WHEN octet_length(created_at) <= $3 THEN created_at END AS created_at, \
                    octet_length(created_at)::BIGINT AS created_at_bytes, \
                    CASE WHEN octet_length(updated_at) <= $3 THEN updated_at END AS updated_at, \
                    octet_length(updated_at)::BIGINT AS updated_at_bytes, revision \
             FROM {} WHERE id = $1 AND ($2::TEXT IS NULL OR flow_path = $2)",
            self.conversations_table
        );
        let row_opt = sqlx::query(sqlx::AssertSqlSafe(sql.to_string()))
            .bind(id)
            .bind(flow_path)
            .bind(i64::try_from(HARD_STORED_CONVERSATION_METADATA_BYTES).unwrap_or(i64::MAX))
            .bind(i64::try_from(HARD_STORED_CONVERSATION_EXECUTION_BYTES).unwrap_or(i64::MAX))
            .bind(i64::try_from(HARD_STORED_CONVERSATION_MESSAGES_BYTES).unwrap_or(i64::MAX))
            .bind(i64::try_from(HARD_STORED_CONVERSATION_MESSAGES).unwrap_or(i64::MAX))
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| {
                IronCrewError::Validation(format!("PostgreSQL get_conversation error: {}", e))
            })?;
        let Some(row) = row_opt else {
            return Ok(None);
        };
        let execution_bytes = stored_bytes(&row, "execution_bytes", "execution")?;
        let messages_bytes = stored_bytes(&row, "messages_bytes", "messages")?;
        let message_count = row
            .try_get::<Option<i64>, _>("message_count")
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL stored conversation message-count decode failed: {error}"
                ))
            })?
            .map(|count| {
                u64::try_from(count).map_err(|_| {
                    IronCrewError::Validation(
                        "PostgreSQL stored conversation has an invalid message count".into(),
                    )
                })
            })
            .transpose()?;
        validate_stored_conversation_envelope(execution_bytes, messages_bytes, message_count)?;
        let execution_json = row
            .try_get::<Option<String>, _>("execution")
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL stored conversation execution identity decode failed: {error}"
                ))
            })?
            .ok_or_else(|| {
                IronCrewError::Validation(
                    "PostgreSQL stored conversation execution identity could not be materialized safely"
                        .into(),
                )
            })?;
        let messages_json = row
            .try_get::<Option<String>, _>("messages")
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL stored conversation messages decode failed: {error}"
                ))
            })?
            .ok_or_else(|| {
                IronCrewError::Validation(
                    "PostgreSQL stored conversation messages could not be materialized safely"
                        .into(),
                )
            })?;
        preflight_conversation_execution_json(&execution_json)?;
        preflight_conversation_messages_json(&messages_json)?;
        let record = ConversationRecord {
            id: row
                .try_get("id")
                .map_err(|e| IronCrewError::Validation(e.to_string()))?,
            flow_name: bounded_metadata(&row, "flow_name", "flow_name_bytes", "flow name")?,
            flow_path: bounded_optional_metadata(
                &row,
                "flow_path",
                "flow_path_bytes",
                "flow path",
            )?,
            agent_name: bounded_metadata(&row, "agent_name", "agent_name_bytes", "agent name")?,
            execution: decode_stored_json(&execution_json, "conversations.execution")?,
            messages: decode_stored_json(&messages_json, "conversations.messages")?,
            created_at: bounded_metadata(
                &row,
                "created_at",
                "created_at_bytes",
                "created timestamp",
            )?,
            updated_at: bounded_metadata(
                &row,
                "updated_at",
                "updated_at_bytes",
                "updated timestamp",
            )?,
            revision: u64::try_from(
                row.try_get::<i64, _>("revision")
                    .map_err(|e| IronCrewError::Validation(e.to_string()))?,
            )
            .map_err(|_| IronCrewError::Validation("Invalid conversation revision".into()))?,
        };
        validate_conversation_record_after_decode(&record)?;
        Ok(Some(record))
    }
}
