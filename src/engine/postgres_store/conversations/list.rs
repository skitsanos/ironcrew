use sqlx::Row;

use crate::engine::conversation_record::{
    HARD_STORED_CONVERSATION_MESSAGES, HARD_STORED_CONVERSATION_MESSAGES_BYTES,
    HARD_STORED_CONVERSATION_METADATA_BYTES,
};
use crate::engine::sessions::ConversationSummary;
use crate::utils::error::{IronCrewError, Result};

use super::super::PostgresStore;
use super::super::codecs::conversation_summary;

impl PostgresStore {
    pub(in crate::engine::postgres_store) async fn list_conversation_records(
        &self,
        flow_path: Option<&str>,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<ConversationSummary>> {
        let sql = format!(
            "SELECT \
                    CASE WHEN octet_length(c.id) <= $4 THEN c.id END AS id, \
                    octet_length(c.id)::BIGINT AS id_bytes, \
                    CASE WHEN c.flow_path IS NULL OR octet_length(c.flow_path) <= $4 \
                         THEN c.flow_path END AS flow_path, \
                    octet_length(c.flow_path)::BIGINT AS flow_path_bytes, \
                    CASE WHEN octet_length(c.agent_name) <= $4 THEN c.agent_name END AS agent_name, \
                    octet_length(c.agent_name)::BIGINT AS agent_name_bytes, \
                    (SELECT COUNT(*) FROM jsonb_array_elements( \
                       CASE WHEN octet_length(c.messages::text) <= $5 \
                         THEN CASE WHEN jsonb_typeof(c.messages) = 'array' \
                           THEN CASE WHEN jsonb_array_length(c.messages)::BIGINT <= $6 \
                             THEN c.messages ELSE '[]'::jsonb END \
                           ELSE '[]'::jsonb END \
                         ELSE '[]'::jsonb END \
                     ) AS message WHERE message->>'role' = 'user') AS turn_count, \
                    octet_length(c.messages::text)::BIGINT AS messages_bytes, \
                    CASE WHEN jsonb_typeof(c.messages) = 'array' \
                         THEN jsonb_array_length(c.messages)::BIGINT END AS message_count, \
                    CASE WHEN octet_length(c.created_at) <= $4 THEN c.created_at END AS created_at, \
                    octet_length(c.created_at)::BIGINT AS created_at_bytes, \
                    CASE WHEN octet_length(c.updated_at) <= $4 THEN c.updated_at END \
                         AS bounded_updated_at, \
                    octet_length(c.updated_at)::BIGINT AS updated_at_bytes \
             FROM {} AS c \
             WHERE ($1::TEXT IS NULL OR c.flow_path = $1) \
             ORDER BY bounded_updated_at DESC \
             LIMIT $2 OFFSET $3",
            self.conversations_table
        );
        let limit_i = if limit == 0 {
            i64::MAX
        } else {
            i64::try_from(limit).unwrap_or(i64::MAX)
        };
        let rows = sqlx::query(sqlx::AssertSqlSafe(sql.to_string()))
            .bind(flow_path)
            .bind(limit_i)
            .bind(i64::try_from(offset).unwrap_or(i64::MAX))
            .bind(i64::try_from(HARD_STORED_CONVERSATION_METADATA_BYTES).unwrap_or(i64::MAX))
            .bind(i64::try_from(HARD_STORED_CONVERSATION_MESSAGES_BYTES).unwrap_or(i64::MAX))
            .bind(i64::try_from(HARD_STORED_CONVERSATION_MESSAGES).unwrap_or(i64::MAX))
            .fetch_all(&self.pool)
            .await
            .map_err(|e| {
                IronCrewError::Validation(format!("PostgreSQL list_conversations error: {}", e))
            })?;
        let mut summaries = Vec::with_capacity(rows.len());
        for row in &rows {
            summaries.push(conversation_summary(row).map_err(|_| {
                IronCrewError::Validation(
                    "PostgreSQL stored conversation summary is corrupt or exceeds hard limits"
                        .into(),
                )
            })?);
        }
        Ok(summaries)
    }

    pub(in crate::engine::postgres_store) async fn count_conversation_records(
        &self,
        flow_path: Option<&str>,
    ) -> Result<u64> {
        let sql = format!(
            "SELECT COUNT(*) FROM {} \
             WHERE ($1::TEXT IS NULL OR flow_path = $1)",
            self.conversations_table
        );
        let row = sqlx::query(sqlx::AssertSqlSafe(sql.to_string()))
            .bind(flow_path)
            .fetch_one(&self.pool)
            .await
            .map_err(|e| {
                IronCrewError::Validation(format!("PostgreSQL count_conversations error: {}", e))
            })?;
        let count: i64 = row
            .try_get(0)
            .map_err(|e| IronCrewError::Validation(format!("Column error: {}", e)))?;
        Ok(count as u64)
    }
}
