use super::*;

pub(super) struct BoundedConversationRow {
    pub(super) usage: Option<String>,
    pub(super) id: String,
    pub(super) flow_name: Option<String>,
    pub(super) flow_name_bytes: i64,
    pub(super) flow_path: Option<String>,
    pub(super) flow_path_bytes: Option<i64>,
    pub(super) agent_name: Option<String>,
    pub(super) agent_name_bytes: i64,
    pub(super) execution: Option<String>,
    pub(super) execution_bytes: i64,
    pub(super) messages: Option<String>,
    pub(super) messages_bytes: i64,
    pub(super) message_count: Option<i64>,
    pub(super) created_at: Option<String>,
    pub(super) created_at_bytes: i64,
    pub(super) updated_at: Option<String>,
    pub(super) updated_at_bytes: i64,
    pub(super) revision: i64,
}

pub(super) struct BoundedConversationSummaryRow {
    pub(super) usage: Option<String>,
    pub(super) id: Option<String>,
    pub(super) id_bytes: i64,
    pub(super) flow_path: Option<String>,
    pub(super) flow_path_bytes: Option<i64>,
    pub(super) agent_name: Option<String>,
    pub(super) agent_name_bytes: i64,
    pub(super) turn_count: i64,
    pub(super) messages_bytes: i64,
    pub(super) message_count: Option<i64>,
    pub(super) created_at: Option<String>,
    pub(super) created_at_bytes: i64,
    pub(super) updated_at: Option<String>,
    pub(super) updated_at_bytes: i64,
}

pub(super) fn sqlite_stored_bytes(value: i64, label: &str) -> Result<u64> {
    u64::try_from(value).map_err(|_| {
        IronCrewError::Validation(format!(
            "SQLite stored conversation {label} has an invalid byte count"
        ))
    })
}

pub(super) fn sqlite_bounded_metadata(
    value: Option<String>,
    bytes: i64,
    label: &str,
) -> Result<String> {
    let bytes = sqlite_stored_bytes(bytes, label)?;
    validate_stored_conversation_metadata_bytes(label, bytes)?;
    value.ok_or_else(|| {
        IronCrewError::Validation(format!(
            "SQLite stored conversation {label} could not be materialized safely"
        ))
    })
}

pub(super) fn sqlite_bounded_optional_metadata(
    value: Option<String>,
    bytes: Option<i64>,
    label: &str,
) -> Result<Option<String>> {
    let Some(bytes) = bytes else {
        return Ok(None);
    };
    sqlite_bounded_metadata(value, bytes, label).map(Some)
}

pub(super) fn sqlite_bounded_conversation_execution(
    value: Option<String>,
    bytes: i64,
    column: usize,
) -> Result<crate::engine::sessions::ConversationExecution> {
    let bytes = sqlite_stored_bytes(bytes, "execution")?;
    crate::engine::conversation_record::validate_stored_conversation_execution_bytes(bytes)?;
    let value = value.ok_or_else(|| {
        IronCrewError::Validation(
            "SQLite stored conversation execution identity could not be materialized safely".into(),
        )
    })?;
    preflight_conversation_execution_json(&value)?;
    decode_stored_json(&value, column).map_err(|error| {
        IronCrewError::Validation(format!(
            "SQLite stored conversation execution identity has an invalid shape: {error}"
        ))
    })
}

pub(super) fn sqlite_conversation_summary(
    row: BoundedConversationSummaryRow,
) -> Result<ConversationSummary> {
    let messages_bytes = sqlite_stored_bytes(row.messages_bytes, "messages")?;
    let message_count = row
        .message_count
        .map(|count| sqlite_stored_bytes(count, "message count"))
        .transpose()?;
    validate_stored_conversation_messages_envelope(messages_bytes, message_count)?;
    let id = sqlite_bounded_metadata(row.id, row.id_bytes, "id")?;
    validate_session_id(&id)?;
    Ok(ConversationSummary {
        usage: crate::engine::session_usage::decode(row.usage.as_deref())?,
        id,
        flow_path: sqlite_bounded_optional_metadata(
            row.flow_path,
            row.flow_path_bytes,
            "flow path",
        )?,
        agent_name: sqlite_bounded_metadata(row.agent_name, row.agent_name_bytes, "agent name")?,
        created_at: sqlite_bounded_metadata(
            row.created_at,
            row.created_at_bytes,
            "created timestamp",
        )?,
        updated_at: sqlite_bounded_metadata(
            row.updated_at,
            row.updated_at_bytes,
            "updated timestamp",
        )?,
        turn_count: usize::try_from(row.turn_count).map_err(|_| {
            IronCrewError::Validation("SQLite conversation turn count is out of range".into())
        })?,
    })
}
