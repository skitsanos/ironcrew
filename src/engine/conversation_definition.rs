//! Deterministic, secret-free identities for durable conversation definitions.

use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::engine::agent::Agent;
use crate::utils::error::{IronCrewError, Result};

const MAX_SERIALIZED_AGENT_BYTES: usize = 4 * 1024 * 1024;
const DEFINITION_DOMAIN: &[u8] = b"ironcrew:conversation-definition:v1";

mod source;
pub use source::{
    ConversationSourceContext, FlowSourceRoles, FlowSourceSnapshot, SnapshotLuaSource,
    capture_flow_source, flow_source_fingerprint,
};

/// Non-secret inputs that determine whether a persisted conversation can resume.
#[derive(Clone, Copy)]
pub struct ConversationDefinition<'a> {
    pub source_fingerprint: &'a str,
    pub agent: &'a Agent,
    pub resolved_model: &'a str,
    pub effective_system_prompt: &'a str,
    pub max_history: usize,
    pub history_max_bytes: usize,
    pub max_tool_rounds: usize,
    /// Canonical hash of the ordered, resolved tool graph available to the
    /// selected agent.
    pub resolved_tools_fingerprint: &'a str,
    /// Canonical hash of the effective, non-secret provider endpoint and
    /// provider-specific request options. Credentials are intentionally absent.
    pub provider_execution_fingerprint: &'a str,
    /// Canonical app-database description (policy + operation digests) when
    /// the postgres.* capability is configured. `None` contributes nothing,
    /// keeping pre-capability fingerprints bit-identical.
    pub app_db: Option<&'a serde_json::Value>,
}

/// Non-secret app-db description (`AppDb::definition()`), stored as Lua
/// app-data by project setup and read back when a conversation is defined.
#[derive(Clone)]
pub struct AppDbFingerprint(pub serde_json::Value);

/// Hash all effective, non-secret inputs that define conversation behavior.
pub fn conversation_definition_fingerprint(input: &ConversationDefinition<'_>) -> Result<String> {
    validate_sha256(input.source_fingerprint)?;
    if input
        .agent
        .temperature
        .is_some_and(|value| !value.is_finite())
    {
        return Err(validation("agent temperature must be finite"));
    }
    let agent = serde_json::to_value(input.agent)
        .map_err(|error| validation(format!("failed to serialize conversation agent: {error}")))?;
    let serialized_agent_bytes = serde_json::to_vec(&agent)
        .map_err(|error| validation(format!("failed to size conversation agent: {error}")))?;
    if serialized_agent_bytes.len() > MAX_SERIALIZED_AGENT_BYTES {
        return Err(validation(format!(
            "serialized conversation agent exceeds {MAX_SERIALIZED_AGENT_BYTES} bytes"
        )));
    }

    let mut digest = FramedDigest::new(DEFINITION_DOMAIN);
    digest.field(b"source_fingerprint", input.source_fingerprint.as_bytes());
    digest.field(b"agent_encoding", b"canonical-json-v1");
    digest.json(&agent);
    digest.field(b"resolved_model", input.resolved_model.as_bytes());
    digest.field(
        b"effective_system_prompt",
        input.effective_system_prompt.as_bytes(),
    );
    digest.field(b"max_history", &(input.max_history as u64).to_be_bytes());
    digest.field(
        b"history_max_bytes",
        &(input.history_max_bytes as u64).to_be_bytes(),
    );
    digest.field(
        b"max_tool_rounds",
        &(input.max_tool_rounds as u64).to_be_bytes(),
    );
    validate_sha256(input.resolved_tools_fingerprint)?;
    digest.field(
        b"resolved_tools_fingerprint",
        input.resolved_tools_fingerprint.as_bytes(),
    );
    validate_sha256(input.provider_execution_fingerprint)?;
    digest.field(
        b"provider_execution_fingerprint",
        input.provider_execution_fingerprint.as_bytes(),
    );
    if let Some(app_db) = input.app_db {
        digest.field(b"app_db_encoding", b"canonical-json-v1");
        digest.json(app_db);
    }
    Ok(digest.finish())
}

fn validate_sha256(value: &str) -> Result<()> {
    let hex = value.strip_prefix("sha256:").unwrap_or_default();
    if hex.len() != 64
        || !hex
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
    {
        return Err(validation("source fingerprint must be canonical sha256"));
    }
    Ok(())
}

pub(super) fn validation(message: impl Into<String>) -> IronCrewError {
    IronCrewError::Validation(message.into())
}

pub(super) struct FramedDigest(Sha256);

impl FramedDigest {
    pub(super) fn new(domain: &[u8]) -> Self {
        let mut digest = Self(Sha256::new());
        digest.frame(domain);
        digest
    }

    fn frame(&mut self, value: &[u8]) {
        self.0.update((value.len() as u64).to_be_bytes());
        self.0.update(value);
    }

    pub(super) fn field(&mut self, label: &[u8], value: &[u8]) {
        self.frame(label);
        self.frame(value);
    }

    pub(super) fn json(&mut self, value: &Value) {
        match value {
            Value::Null => self.field(b"json_type", b"null"),
            Value::Bool(value) => {
                self.field(b"json_type", b"bool");
                self.frame(if *value { b"true" } else { b"false" });
            }
            Value::Number(value) => {
                self.field(b"json_type", b"number");
                self.frame(value.to_string().as_bytes());
            }
            Value::String(value) => {
                self.field(b"json_type", b"string");
                self.frame(value.as_bytes());
            }
            Value::Array(values) => {
                self.field(b"json_type", b"array");
                self.frame(&(values.len() as u64).to_be_bytes());
                for value in values {
                    self.json(value);
                }
            }
            Value::Object(values) => {
                self.field(b"json_type", b"object");
                self.frame(&(values.len() as u64).to_be_bytes());
                let mut keys = values.keys().collect::<Vec<_>>();
                keys.sort_unstable();
                for key in keys {
                    self.frame(key.as_bytes());
                    self.json(&values[key]);
                }
            }
        }
    }

    pub(super) fn finish(self) -> String {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut output = String::from("sha256:");
        for byte in self.0.finalize() {
            output.push(HEX[usize::from(byte >> 4)] as char);
            output.push(HEX[usize::from(byte & 0x0f)] as char);
        }
        output
    }
}

#[cfg(test)]
mod tests;
