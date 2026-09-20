use sqlx::{Row, postgres::PgRow};

use crate::engine::conversation_json::preflight_conversation_execution_json;
use crate::engine::conversation_record::{
    validate_stored_conversation_execution_bytes, validate_stored_conversation_messages_envelope,
    validate_stored_conversation_metadata_bytes,
};
use crate::engine::sessions::{ConversationExecution, ConversationSummary, validate_session_id};
use crate::utils::error::{IronCrewError, Result};

use super::decode_stored_json;

pub(in crate::engine::postgres_store) fn stored_bytes(
    row: &PgRow,
    column: &str,
    label: &str,
) -> Result<u64> {
    let value = row.try_get::<i64, _>(column).map_err(|error| {
        IronCrewError::Validation(format!(
            "PostgreSQL stored conversation {label} byte-count decode failed: {error}"
        ))
    })?;
    u64::try_from(value).map_err(|_| {
        IronCrewError::Validation(format!(
            "PostgreSQL stored conversation {label} has an invalid byte count"
        ))
    })
}

pub(in crate::engine::postgres_store) fn bounded_metadata(
    row: &PgRow,
    value_column: &str,
    bytes_column: &str,
    label: &str,
) -> Result<String> {
    let bytes = stored_bytes(row, bytes_column, label)?;
    validate_stored_conversation_metadata_bytes(label, bytes)?;
    row.try_get::<Option<String>, _>(value_column)
        .map_err(|error| {
            IronCrewError::Validation(format!(
                "PostgreSQL stored conversation {label} decode failed: {error}"
            ))
        })?
        .ok_or_else(|| {
            IronCrewError::Validation(format!(
                "PostgreSQL stored conversation {label} could not be materialized safely"
            ))
        })
}

pub(in crate::engine::postgres_store) fn bounded_optional_metadata(
    row: &PgRow,
    value_column: &str,
    bytes_column: &str,
    label: &str,
) -> Result<Option<String>> {
    let bytes = row
        .try_get::<Option<i64>, _>(bytes_column)
        .map_err(|error| {
            IronCrewError::Validation(format!(
                "PostgreSQL stored conversation {label} byte-count decode failed: {error}"
            ))
        })?;
    let Some(bytes) = bytes else {
        return Ok(None);
    };
    let bytes = u64::try_from(bytes).map_err(|_| {
        IronCrewError::Validation(format!(
            "PostgreSQL stored conversation {label} has an invalid byte count"
        ))
    })?;
    validate_stored_conversation_metadata_bytes(label, bytes)?;
    row.try_get::<Option<String>, _>(value_column)
        .map_err(|error| {
            IronCrewError::Validation(format!(
                "PostgreSQL stored conversation {label} decode failed: {error}"
            ))
        })?
        .map(Some)
        .ok_or_else(|| {
            IronCrewError::Validation(format!(
                "PostgreSQL stored conversation {label} could not be materialized safely"
            ))
        })
}

pub(in crate::engine::postgres_store) fn bounded_conversation_execution(
    row: &PgRow,
) -> Result<ConversationExecution> {
    let bytes = stored_bytes(row, "execution_bytes", "execution")?;
    validate_stored_conversation_execution_bytes(bytes)?;
    let execution = row
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
    preflight_conversation_execution_json(&execution)?;
    decode_stored_json(&execution, "conversations.execution")
}

pub(in crate::engine::postgres_store) fn conversation_summary(
    row: &PgRow,
) -> Result<ConversationSummary> {
    let messages_bytes = stored_bytes(row, "messages_bytes", "messages")?;
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
    validate_stored_conversation_messages_envelope(messages_bytes, message_count)?;
    let id = bounded_metadata(row, "id", "id_bytes", "id")?;
    validate_session_id(&id)?;
    let turn_count = row.try_get::<i64, _>("turn_count").map_err(|error| {
        IronCrewError::Validation(format!(
            "PostgreSQL stored conversation turn-count decode failed: {error}"
        ))
    })?;
    Ok(ConversationSummary {
        id,
        flow_path: bounded_optional_metadata(row, "flow_path", "flow_path_bytes", "flow path")?,
        agent_name: bounded_metadata(row, "agent_name", "agent_name_bytes", "agent name")?,
        created_at: bounded_metadata(row, "created_at", "created_at_bytes", "created timestamp")?,
        updated_at: bounded_metadata(
            row,
            "bounded_updated_at",
            "updated_at_bytes",
            "updated timestamp",
        )?,
        turn_count: usize::try_from(turn_count).map_err(|_| {
            IronCrewError::Validation("PostgreSQL conversation turn count is out of range".into())
        })?,
    })
}
