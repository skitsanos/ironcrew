//! Persistent session records — conversations and dialogs that can be
//! resumed across `ironcrew run` invocations or API requests.
//!
//! Stored via the `StateStore` trait alongside run history. Each record
//! is keyed by a user-provided (or auto-generated) stable ID. See
//! `docs/crews.md#cross-run-persistence` for the full Lua API.

use serde::{Deserialize, Serialize};

use crate::llm::provider::ChatMessage;
use crate::lua::dialog::DialogTurn;
use crate::utils::error::{IronCrewError, Result};

/// Persistent snapshot of a `crew:conversation({id = "..."})` session.
/// Restored by re-opening the conversation with the same `id`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationRecord {
    pub id: String,
    pub flow_name: String,
    pub agent_name: String,
    pub messages: Vec<ChatMessage>,
    pub created_at: String,
    pub updated_at: String,
}

/// Persistent snapshot of a `crew:dialog({id = "..."})` session.
/// Captures enough state to resume the dialog from the point where it
/// was last saved — the transcript, turn index, and any custom stop state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DialogStateRecord {
    pub id: String,
    pub flow_name: String,
    /// Agents participating in the dialog, in turn order.
    pub agent_names: Vec<String>,
    /// Kickoff message for the first turn.
    pub starter: String,
    pub transcript: Vec<DialogTurn>,
    /// Index of the next turn to run (so a resumed dialog picks up where
    /// it left off). Equal to `transcript.len()` unless custom speaker
    /// selection has been used — we don't assume they match.
    pub next_index: usize,
    #[serde(default)]
    pub stopped: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

/// Validate a user-provided session ID.
///
/// Accepts ASCII alphanumeric characters plus `-`, `_`, and `.`, with a
/// length between 1 and 128 characters. The restriction prevents path
/// traversal against the JSON backend (`../`) and any SQL oddness against
/// the SQLite/PostgreSQL backends. Any violation surfaces as a clear
/// validation error before the ID reaches the store.
pub fn validate_session_id(id: &str) -> Result<()> {
    if id.is_empty() {
        return Err(IronCrewError::Validation(
            "session id must not be empty".into(),
        ));
    }
    if id.len() > 128 {
        return Err(IronCrewError::Validation(format!(
            "session id '{}' is too long (max 128 chars)",
            id
        )));
    }
    for (i, ch) in id.char_indices() {
        if !(ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' || ch == '.') {
            return Err(IronCrewError::Validation(format!(
                "session id '{}' contains invalid character '{}' at position {} \
                 (allowed: letters, digits, '-', '_', '.')",
                id, ch, i
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_alphanumeric() {
        assert!(validate_session_id("abc123").is_ok());
        assert!(validate_session_id("ABC").is_ok());
    }

    #[test]
    fn accepts_dash_underscore_dot() {
        assert!(validate_session_id("chat-2024_01.v2").is_ok());
    }

    #[test]
    fn rejects_empty() {
        assert!(validate_session_id("").is_err());
    }

    #[test]
    fn rejects_path_traversal() {
        assert!(validate_session_id("../etc/passwd").is_err());
        assert!(validate_session_id("foo/bar").is_err());
    }

    #[test]
    fn rejects_spaces() {
        assert!(validate_session_id("hello world").is_err());
    }

    #[test]
    fn rejects_sql_metacharacters() {
        assert!(validate_session_id("foo';DROP TABLE runs;--").is_err());
    }

    #[test]
    fn rejects_oversize() {
        let long = "a".repeat(129);
        assert!(validate_session_id(&long).is_err());
    }

    #[test]
    fn accepts_max_length() {
        let max = "a".repeat(128);
        assert!(validate_session_id(&max).is_ok());
    }
}
