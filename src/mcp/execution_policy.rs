use std::io::Write;
use std::time::Duration;

use serde::Serialize;
use serde_json::{Value, json};

use crate::utils::error::{IronCrewError, Result};

const DEFAULT_CALL_TIMEOUT_SECS: u64 = 60;
const MAX_CALL_TIMEOUT_SECS: u64 = 3_600;
const DEFAULT_MAX_ARGUMENT_BYTES: usize = 256 * 1024;
const HARD_MAX_ARGUMENT_BYTES: usize = 4 * 1024 * 1024;
const DEFAULT_MAX_MRTR_ROUNDS: usize = 10;
const HARD_MAX_MRTR_ROUNDS: usize = 32;
const DEFAULT_MAX_REQUEST_STATE_BYTES: usize = 64 * 1024;
const HARD_MAX_REQUEST_STATE_BYTES: usize = 1024 * 1024;
const DEFAULT_MAX_INBOUND_MESSAGE_BYTES: usize = 1024 * 1024;
const HARD_MAX_INBOUND_MESSAGE_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct McpCallPolicy {
    argument_max_bytes: usize,
    inbound_message_max_bytes: usize,
    max_mrtr_rounds: usize,
    request_state_max_bytes: usize,
    timeout_secs: u64,
}

impl McpCallPolicy {
    pub(super) fn capture() -> Result<Self> {
        Self::capture_from(|name| std::env::var(name).ok())
    }

    fn capture_from(read: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let argument_max_bytes = parse_usize(
            "IRONCREW_MCP_TOOL_ARGUMENT_MAX_BYTES",
            read("IRONCREW_MCP_TOOL_ARGUMENT_MAX_BYTES"),
            DEFAULT_MAX_ARGUMENT_BYTES,
            HARD_MAX_ARGUMENT_BYTES,
        )?;
        let timeout_secs = parse_u64(
            "IRONCREW_MCP_CALL_TIMEOUT_SECS",
            read("IRONCREW_MCP_CALL_TIMEOUT_SECS"),
            DEFAULT_CALL_TIMEOUT_SECS,
            MAX_CALL_TIMEOUT_SECS,
        )?;
        let max_mrtr_rounds = parse_usize(
            "IRONCREW_MCP_MAX_MRTR_ROUNDS",
            read("IRONCREW_MCP_MAX_MRTR_ROUNDS"),
            DEFAULT_MAX_MRTR_ROUNDS,
            HARD_MAX_MRTR_ROUNDS,
        )?;
        let request_state_max_bytes = parse_usize(
            "IRONCREW_MCP_MAX_REQUEST_STATE_BYTES",
            read("IRONCREW_MCP_MAX_REQUEST_STATE_BYTES"),
            DEFAULT_MAX_REQUEST_STATE_BYTES,
            HARD_MAX_REQUEST_STATE_BYTES,
        )?;
        let inbound_message_max_bytes = parse_usize(
            "IRONCREW_MCP_MAX_INBOUND_MESSAGE_BYTES",
            read("IRONCREW_MCP_MAX_INBOUND_MESSAGE_BYTES"),
            DEFAULT_MAX_INBOUND_MESSAGE_BYTES,
            HARD_MAX_INBOUND_MESSAGE_BYTES,
        )?;
        Ok(Self {
            argument_max_bytes,
            inbound_message_max_bytes,
            max_mrtr_rounds,
            request_state_max_bytes,
            timeout_secs,
        })
    }

    pub(super) fn validate_arguments(&self, args: &Value) -> Result<()> {
        ensure_serialized_size(args, self.argument_max_bytes, "MCP tool arguments")
    }

    pub(super) fn timeout(&self) -> Duration {
        Duration::from_secs(self.timeout_secs)
    }

    pub(super) fn max_mrtr_rounds(&self) -> usize {
        self.max_mrtr_rounds
    }

    pub(super) fn inbound_message_max_bytes(&self) -> usize {
        self.inbound_message_max_bytes
    }

    pub(super) fn validate_request_state(&self, state: &str) -> Result<()> {
        if state.len() > self.request_state_max_bytes {
            return Err(mcp_error(format!(
                "MCP requestState exceeds {} bytes",
                self.request_state_max_bytes
            )));
        }
        Ok(())
    }

    pub(super) fn definition(&self) -> Value {
        json!({
            "argument_max_bytes": self.argument_max_bytes,
            "inbound_message_max_bytes": self.inbound_message_max_bytes,
            "max_mrtr_rounds": self.max_mrtr_rounds,
            "request_state_max_bytes": self.request_state_max_bytes,
            "timeout_secs": self.timeout_secs,
        })
    }
}

struct SerializedSizeLimiter {
    bytes: usize,
    limit: usize,
}

impl Write for SerializedSizeLimiter {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        self.bytes = self
            .bytes
            .checked_add(buffer.len())
            .ok_or_else(|| std::io::Error::other("serialized size overflow"))?;
        if self.bytes > self.limit {
            return Err(std::io::Error::new(
                std::io::ErrorKind::FileTooLarge,
                "serialized value exceeds limit",
            ));
        }
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

pub(super) fn ensure_serialized_size<T: Serialize>(
    value: &T,
    limit: usize,
    label: &str,
) -> Result<()> {
    let mut writer = SerializedSizeLimiter { bytes: 0, limit };
    serde_json::to_writer(&mut writer, value)
        .map_err(|error| mcp_error(format!("{label} exceeds {limit} bytes: {error}")))
}

fn parse_u64(name: &str, raw: Option<String>, default: u64, max: u64) -> Result<u64> {
    let value = raw
        .map(|raw| {
            raw.parse::<u64>()
                .map_err(|_| mcp_error(format!("{name} must be an integer from 1 to {max}")))
        })
        .transpose()?
        .unwrap_or(default);
    if !(1..=max).contains(&value) {
        return Err(mcp_error(format!("{name} must be from 1 to {max}")));
    }
    Ok(value)
}

fn parse_usize(name: &str, raw: Option<String>, default: usize, max: usize) -> Result<usize> {
    let value = raw
        .map(|raw| {
            raw.parse::<usize>()
                .map_err(|_| mcp_error(format!("{name} must be an integer from 1 to {max}")))
        })
        .transpose()?
        .unwrap_or(default);
    if !(1..=max).contains(&value) {
        return Err(mcp_error(format!("{name} must be from 1 to {max}")));
    }
    Ok(value)
}

fn mcp_error(message: impl Into<String>) -> IronCrewError {
    IronCrewError::Mcp {
        server: String::new(),
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    #[test]
    fn captured_policy_is_immutable_when_configuration_drifts() {
        let args = json!({"value": "captured"});
        let exact_bytes = serde_json::to_vec(&args).unwrap().len();
        let configured_argument_bytes = Cell::new(exact_bytes);
        let configured_mrtr_rounds = Cell::new(7_usize);
        let configured_request_state_bytes = Cell::new(5_usize);
        let configured_timeout_secs = Cell::new(17_u64);
        let capture = || {
            McpCallPolicy::capture_from(|name| match name {
                "IRONCREW_MCP_TOOL_ARGUMENT_MAX_BYTES" => {
                    Some(configured_argument_bytes.get().to_string())
                }
                "IRONCREW_MCP_CALL_TIMEOUT_SECS" => Some(configured_timeout_secs.get().to_string()),
                "IRONCREW_MCP_MAX_MRTR_ROUNDS" => Some(configured_mrtr_rounds.get().to_string()),
                "IRONCREW_MCP_MAX_REQUEST_STATE_BYTES" => {
                    Some(configured_request_state_bytes.get().to_string())
                }
                _ => None,
            })
        };

        let captured = capture().unwrap();
        configured_argument_bytes.set(exact_bytes - 1);
        configured_mrtr_rounds.set(1);
        configured_request_state_bytes.set(1);
        configured_timeout_secs.set(1);

        captured.validate_arguments(&args).unwrap();
        captured.validate_request_state("state").unwrap();
        assert_eq!(captured.max_mrtr_rounds(), 7);
        assert_eq!(captured.timeout(), Duration::from_secs(17));
        assert_eq!(
            captured.definition(),
            json!({
                "argument_max_bytes": exact_bytes,
                "inbound_message_max_bytes": DEFAULT_MAX_INBOUND_MESSAGE_BYTES,
                "max_mrtr_rounds": 7,
                "request_state_max_bytes": 5,
                "timeout_secs": 17,
            })
        );

        let recaptured = capture().unwrap();
        assert!(recaptured.validate_arguments(&args).is_err());
        assert!(recaptured.validate_request_state("state").is_err());
        assert_eq!(recaptured.max_mrtr_rounds(), 1);
        assert_eq!(recaptured.timeout(), Duration::from_secs(1));
    }

    #[test]
    fn captured_policy_rejects_oversized_arguments_at_execution_boundary() {
        let args = json!({"value": "too large"});
        let exact_bytes = serde_json::to_vec(&args).unwrap().len();
        let policy = McpCallPolicy::capture_from(|name| match name {
            "IRONCREW_MCP_TOOL_ARGUMENT_MAX_BYTES" => Some((exact_bytes - 1).to_string()),
            "IRONCREW_MCP_CALL_TIMEOUT_SECS" => Some("9".into()),
            _ => None,
        })
        .unwrap();

        let error = policy.validate_arguments(&args).unwrap_err().to_string();
        assert!(error.contains("MCP tool arguments exceeds"));
        assert_eq!(policy.timeout(), Duration::from_secs(9));
    }

    #[test]
    fn invalid_call_policy_fails_during_capture() {
        let error = McpCallPolicy::capture_from(|name| {
            (name == "IRONCREW_MCP_CALL_TIMEOUT_SECS").then(|| "0".into())
        })
        .unwrap_err()
        .to_string();

        assert!(error.contains("IRONCREW_MCP_CALL_TIMEOUT_SECS must be from 1 to 3600"));
    }

    #[test]
    fn request_state_limit_accepts_exact_bytes_and_rejects_one_more() {
        let policy = McpCallPolicy::capture_from(|name| {
            (name == "IRONCREW_MCP_MAX_REQUEST_STATE_BYTES").then(|| "4".into())
        })
        .unwrap();

        policy.validate_request_state("🙂").unwrap();
        let error = policy
            .validate_request_state("🙂a")
            .unwrap_err()
            .to_string();
        assert!(error.contains("MCP requestState exceeds 4 bytes"));
    }

    #[test]
    fn invalid_mrtr_limits_fail_during_capture() {
        for (name, value, expected) in [
            (
                "IRONCREW_MCP_MAX_MRTR_ROUNDS",
                "0",
                "IRONCREW_MCP_MAX_MRTR_ROUNDS must be from 1 to 32",
            ),
            (
                "IRONCREW_MCP_MAX_REQUEST_STATE_BYTES",
                "1048577",
                "IRONCREW_MCP_MAX_REQUEST_STATE_BYTES must be from 1 to 1048576",
            ),
        ] {
            let error = McpCallPolicy::capture_from(|candidate| {
                (candidate == name).then(|| value.to_string())
            })
            .unwrap_err()
            .to_string();
            assert!(error.contains(expected), "got: {error}");
        }
    }
}
