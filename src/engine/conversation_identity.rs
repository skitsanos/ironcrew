//! Durable identity and persisted record types for conversations.

use serde::{Deserialize, Serialize};

use crate::llm::provider::{
    ChatMessage, HARD_CHAT_HISTORY_MAX_BYTES, HARD_CHAT_HISTORY_MAX_MESSAGES,
};
use crate::utils::error::{IronCrewError, Result};

pub const CONVERSATION_EXECUTION_SCHEMA_VERSION: u32 = 1;

/// Identity required to resume a persistent conversation safely.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConversationExecution {
    #[serde(default)]
    pub schema_version: u32,
    #[serde(default)]
    pub incarnation_id: String,
    #[serde(default)]
    pub source_fingerprint: String,
    #[serde(default)]
    pub definition_fingerprint: String,
    #[serde(default)]
    pub max_history: usize,
    #[serde(default)]
    pub history_max_bytes: usize,
}

impl ConversationExecution {
    pub fn new(
        source_fingerprint: String,
        definition_fingerprint: String,
        max_history: usize,
        history_max_bytes: usize,
    ) -> Result<Self> {
        let execution = Self {
            schema_version: CONVERSATION_EXECUTION_SCHEMA_VERSION,
            incarnation_id: uuid::Uuid::new_v4().to_string(),
            source_fingerprint,
            definition_fingerprint,
            max_history,
            history_max_bytes,
        };
        execution.validate()?;
        Ok(execution)
    }

    pub fn validate(&self) -> Result<()> {
        if self.schema_version != CONVERSATION_EXECUTION_SCHEMA_VERSION {
            return Err(conflict("legacy or unsupported execution identity"));
        }
        let incarnation = uuid::Uuid::parse_str(&self.incarnation_id)
            .map_err(|_| conflict("invalid durable incarnation"))?;
        if incarnation.is_nil() || incarnation.hyphenated().to_string() != self.incarnation_id {
            return Err(conflict("non-canonical durable incarnation"));
        }
        for (label, value) in [
            ("source", self.source_fingerprint.as_str()),
            ("definition", self.definition_fingerprint.as_str()),
        ] {
            let digest = value.strip_prefix("sha256:").unwrap_or_default();
            if digest.len() != 64
                || !digest
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            {
                return Err(conflict(&format!("invalid {label} fingerprint")));
            }
        }
        if !(1..=HARD_CHAT_HISTORY_MAX_MESSAGES).contains(&self.max_history)
            || !(1..=HARD_CHAT_HISTORY_MAX_BYTES).contains(&self.history_max_bytes)
        {
            return Err(conflict("invalid durable transcript limits"));
        }
        Ok(())
    }
}

fn conflict(reason: &str) -> IronCrewError {
    IronCrewError::Conflict(format!(
        "Conversation has a {reason}; export its history, delete it, and start a new conversation"
    ))
}

pub fn conversation_mutation_scope(flow: &str, id: &str, incarnation_id: &str) -> String {
    format!("conversation.message:v2:{flow}:{id}:{incarnation_id}")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationRecord {
    pub id: String,
    pub flow_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flow_path: Option<String>,
    pub agent_name: String,
    #[serde(default)]
    pub execution: ConversationExecution,
    pub messages: Vec<ChatMessage>,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default)]
    pub revision: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationSummary {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flow_path: Option<String>,
    pub agent_name: String,
    pub created_at: String,
    pub updated_at: String,
    pub turn_count: usize,
}

impl From<&ConversationRecord> for ConversationSummary {
    fn from(record: &ConversationRecord) -> Self {
        Self {
            id: record.id.clone(),
            flow_path: record.flow_path.clone(),
            agent_name: record.agent_name.clone(),
            created_at: record.created_at.clone(),
            updated_at: record.updated_at.clone(),
            turn_count: record
                .messages
                .iter()
                .filter(|message| message.role == "user")
                .count(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn execution() -> ConversationExecution {
        ConversationExecution::new(
            format!("sha256:{}", "a".repeat(64)),
            format!("sha256:{}", "b".repeat(64)),
            50,
            1024,
        )
        .unwrap()
    }

    #[test]
    fn execution_identity_and_limits_are_canonical() {
        let valid = execution();
        valid.validate().unwrap();

        let mut invalid = valid.clone();
        invalid.incarnation_id = invalid.incarnation_id.replace('-', "");
        assert!(invalid.validate().is_err());
        invalid = valid.clone();
        invalid.incarnation_id = uuid::Uuid::nil().to_string();
        assert!(invalid.validate().is_err());
        invalid = valid.clone();
        invalid.source_fingerprint = format!("sha256:{}", "A".repeat(64));
        assert!(invalid.validate().is_err());
        invalid = valid.clone();
        invalid.max_history = HARD_CHAT_HISTORY_MAX_MESSAGES.saturating_add(1);
        assert!(invalid.validate().is_err());
        invalid = valid;
        invalid.history_max_bytes = HARD_CHAT_HISTORY_MAX_BYTES.saturating_add(1);
        assert!(invalid.validate().is_err());
    }

    #[test]
    fn mutation_scope_changes_with_incarnation() {
        let first = execution();
        let second = execution();
        assert_ne!(
            conversation_mutation_scope("flow-a", "chat", &first.incarnation_id),
            conversation_mutation_scope("flow-a", "chat", &second.incarnation_id)
        );
    }
}
