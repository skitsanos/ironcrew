//! Bounded storage boundary for durable conversation records.

use std::io::{self, Write};

use serde::Serialize;

use super::sessions::{ConversationExecution, ConversationRecord, validate_session_id};
use crate::llm::provider::{
    ChatMessage, HARD_CHAT_HISTORY_MAX_BYTES, HARD_CHAT_HISTORY_MAX_MESSAGES,
    chat_history_estimated_bytes, validate_chat_history,
};
use crate::utils::error::{IronCrewError, Result};

/// Hard serialized-size ceiling applied before SQL backends materialize a
/// conversation execution identity.
pub const HARD_STORED_CONVERSATION_EXECUTION_BYTES: usize = 16 * 1024;
/// Hard serialized-size ceiling applied before SQL backends materialize a
/// conversation transcript.
pub const HARD_STORED_CONVERSATION_MESSAGES_BYTES: usize = HARD_CHAT_HISTORY_MAX_BYTES;
/// A valid conversation has one system message in addition to the configured
/// non-system message budget.
pub const HARD_STORED_CONVERSATION_MESSAGES: usize = HARD_CHAT_HISTORY_MAX_MESSAGES + 1;
/// Defensive ceiling for each non-transcript text field in a stored row.
pub const HARD_STORED_CONVERSATION_METADATA_BYTES: usize = 64 * 1024;

struct BoundedJsonWriter {
    bytes: Vec<u8>,
    max_bytes: usize,
}

impl Write for BoundedJsonWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        if self.bytes.len().saturating_add(buffer.len()) > self.max_bytes {
            return Err(io::Error::new(
                io::ErrorKind::FileTooLarge,
                "serialized conversation field exceeds its hard byte limit",
            ));
        }
        self.bytes.extend_from_slice(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn serialize_bounded<T: Serialize + ?Sized>(
    value: &T,
    label: &str,
    max_bytes: usize,
) -> Result<String> {
    let mut writer = BoundedJsonWriter {
        bytes: Vec::with_capacity(max_bytes.min(64 * 1024)),
        max_bytes,
    };
    serde_json::to_writer(&mut writer, value).map_err(|error| {
        IronCrewError::Validation(format!(
            "Failed to serialize conversation {label} within {max_bytes} bytes: {error}"
        ))
    })?;
    String::from_utf8(writer.bytes).map_err(|error| {
        IronCrewError::Validation(format!(
            "Serialized conversation {label} was not valid UTF-8: {error}"
        ))
    })
}

pub fn serialize_conversation_execution(execution: &ConversationExecution) -> Result<String> {
    let json = serialize_bounded(
        execution,
        "execution identity",
        HARD_STORED_CONVERSATION_EXECUTION_BYTES,
    )?;
    super::conversation_json::preflight_conversation_execution_json(&json)?;
    Ok(json)
}

pub fn serialize_conversation_messages(messages: &[ChatMessage]) -> Result<String> {
    let json = serialize_bounded(
        messages,
        "messages",
        HARD_STORED_CONVERSATION_MESSAGES_BYTES,
    )?;
    super::conversation_json::preflight_conversation_messages_json(&json)?;
    Ok(json)
}

pub fn validate_stored_conversation_metadata_bytes(label: &str, bytes: u64) -> Result<()> {
    if bytes > HARD_STORED_CONVERSATION_METADATA_BYTES as u64 {
        return Err(IronCrewError::Validation(format!(
            "Stored conversation {label} exceeds the hard metadata byte limit"
        )));
    }
    Ok(())
}

/// Validate the size and top-level shape measured by a SQL backend before it
/// returns the corresponding JSON strings to Rust.
pub fn validate_stored_conversation_envelope(
    execution_bytes: u64,
    messages_bytes: u64,
    message_count: Option<u64>,
) -> Result<()> {
    validate_stored_conversation_execution_bytes(execution_bytes)?;
    validate_stored_conversation_messages_envelope(messages_bytes, message_count)
}

pub fn validate_stored_conversation_messages_envelope(
    messages_bytes: u64,
    message_count: Option<u64>,
) -> Result<()> {
    if messages_bytes > HARD_STORED_CONVERSATION_MESSAGES_BYTES as u64 {
        return Err(IronCrewError::Validation(
            "Stored conversation messages exceed the hard byte limit".into(),
        ));
    }
    let message_count = message_count.ok_or_else(|| {
        IronCrewError::Validation("Stored conversation messages must be a JSON array".into())
    })?;
    if message_count > HARD_STORED_CONVERSATION_MESSAGES as u64 {
        return Err(IronCrewError::Validation(
            "Stored conversation message count exceeds the hard limit".into(),
        ));
    }
    Ok(())
}

pub fn validate_stored_conversation_execution_bytes(execution_bytes: u64) -> Result<()> {
    if execution_bytes > HARD_STORED_CONVERSATION_EXECUTION_BYTES as u64 {
        return Err(IronCrewError::Validation(
            "Stored conversation execution identity exceeds the hard byte limit".into(),
        ));
    }
    Ok(())
}

fn validate_metadata(record: &ConversationRecord) -> Result<()> {
    for (label, value) in [
        ("flow name", record.flow_name.as_str()),
        ("agent name", record.agent_name.as_str()),
        ("created timestamp", record.created_at.as_str()),
        ("updated timestamp", record.updated_at.as_str()),
    ] {
        validate_stored_conversation_metadata_bytes(label, value.len() as u64)?;
    }
    if let Some(flow_path) = record.flow_path.as_deref() {
        validate_stored_conversation_metadata_bytes("flow path", flow_path.len() as u64)?;
    }
    Ok(())
}

/// Enforce durable identity and transcript invariants before every store write.
pub fn validate_conversation_record_for_write(record: &ConversationRecord) -> Result<()> {
    validate_session_id(&record.id)?;
    validate_metadata(record)?;
    record.execution.validate()?;
    validate_chat_history(
        &record.messages,
        record.execution.max_history,
        record.execution.history_max_bytes,
        true,
    )
    .map_err(|error| {
        IronCrewError::Validation(format!(
            "Conversation '{}' has invalid persisted history: {error}",
            record.id
        ))
    })
}

/// Validate a bounded, decoded store row. Legacy or future execution identities
/// remain exportable, but current identities must satisfy their persisted
/// transcript limits before the record can be adopted.
pub fn validate_conversation_record_after_decode(record: &ConversationRecord) -> Result<()> {
    validate_session_id(&record.id)?;
    validate_metadata(record)?;
    if record.messages.len() > HARD_STORED_CONVERSATION_MESSAGES {
        return Err(IronCrewError::Validation(
            "Stored conversation message count exceeds the hard limit".into(),
        ));
    }
    if chat_history_estimated_bytes(&record.messages) > HARD_CHAT_HISTORY_MAX_BYTES {
        return Err(IronCrewError::Validation(
            "Stored conversation history exceeds the hard in-memory byte limit".into(),
        ));
    }
    if record.execution.validate().is_ok() {
        validate_chat_history(
            &record.messages,
            record.execution.max_history,
            record.execution.history_max_bytes,
            true,
        )
        .map_err(|error| {
            IronCrewError::Validation(format!(
                "Conversation '{}' has invalid persisted history: {error}",
                record.id
            ))
        })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::sessions::CONVERSATION_EXECUTION_SCHEMA_VERSION;

    fn record() -> ConversationRecord {
        ConversationRecord {
            id: "bounded-chat".into(),
            flow_name: "chat".into(),
            flow_path: Some("chat".into()),
            agent_name: "assistant".into(),
            execution: ConversationExecution {
                schema_version: CONVERSATION_EXECUTION_SCHEMA_VERSION,
                incarnation_id: "00000000-0000-4000-8000-000000000001".into(),
                source_fingerprint: format!("sha256:{}", "1".repeat(64)),
                definition_fingerprint: format!("sha256:{}", "2".repeat(64)),
                max_history: 2,
                history_max_bytes: 1024,
            },
            messages: vec![ChatMessage::system("system")],
            created_at: "2026-08-11T00:00:00Z".into(),
            updated_at: "2026-08-11T00:00:00Z".into(),
            revision: 0,
        }
    }

    #[test]
    fn writes_enforce_persisted_limits() {
        let mut record = record();
        record.messages.push(ChatMessage::user("one"));
        record
            .messages
            .push(ChatMessage::assistant(Some("two".into()), None));
        record.messages.push(ChatMessage::user("three"));
        assert!(validate_conversation_record_for_write(&record).is_err());
    }

    #[test]
    fn legacy_records_remain_exportable_after_bounded_decode() {
        let mut record = record();
        record.execution = ConversationExecution::default();
        record.messages.clear();
        validate_conversation_record_after_decode(&record).unwrap();
        assert!(validate_conversation_record_for_write(&record).is_err());
    }

    #[test]
    fn storage_envelope_rejects_oversized_serialized_messages() {
        let error = validate_stored_conversation_envelope(
            2,
            HARD_STORED_CONVERSATION_MESSAGES_BYTES as u64 + 1,
            Some(0),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("messages exceed the hard byte limit"));
    }
}
