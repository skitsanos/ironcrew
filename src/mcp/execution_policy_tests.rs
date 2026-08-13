use std::{cell::Cell, time::Duration};

use serde_json::{Value, json};

use super::execution_policy::{DEFAULT_MAX_INBOUND_MESSAGE_BYTES, McpCallPolicy};

#[test]
fn captured_policy_is_immutable_when_configuration_drifts() {
    let args = json!({"value": "captured"});
    let exact_bytes = serde_json::to_vec(&args).unwrap().len();
    let argument_bytes = Cell::new(exact_bytes);
    let rounds = Cell::new(7_usize);
    let state_bytes = Cell::new(5_usize);
    let timeout_secs = Cell::new(17_u64);
    let capture = || {
        McpCallPolicy::capture_from(|name| match name {
            "IRONCREW_MCP_TOOL_ARGUMENT_MAX_BYTES" => Some(argument_bytes.get().to_string()),
            "IRONCREW_MCP_CALL_TIMEOUT_SECS" => Some(timeout_secs.get().to_string()),
            "IRONCREW_MCP_MAX_MRTR_ROUNDS" => Some(rounds.get().to_string()),
            "IRONCREW_MCP_MAX_REQUEST_STATE_BYTES" => Some(state_bytes.get().to_string()),
            _ => None,
        })
    };
    let captured = capture().unwrap();
    argument_bytes.set(exact_bytes - 1);
    rounds.set(1);
    state_bytes.set(1);
    timeout_secs.set(1);

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
    assert!(capture().unwrap().validate_arguments(&args).is_err());
}

#[test]
fn captured_policy_rejects_oversized_arguments_and_state() {
    let args = json!({"value": "too large"});
    let exact_bytes = serde_json::to_vec(&args).unwrap().len();
    let policy = policy(&[
        ("IRONCREW_MCP_TOOL_ARGUMENT_MAX_BYTES", exact_bytes - 1),
        ("IRONCREW_MCP_MAX_REQUEST_STATE_BYTES", 4),
    ]);
    assert!(policy.validate_arguments(&args).is_err());
    policy.validate_request_state("🙂").unwrap();
    assert!(policy.validate_request_state("🙂a").is_err());
}

#[test]
fn invalid_policy_limits_fail_during_capture() {
    for (name, value, expected) in [
        (
            "IRONCREW_MCP_CALL_TIMEOUT_SECS",
            "0",
            "must be from 1 to 3600",
        ),
        ("IRONCREW_MCP_MAX_MRTR_ROUNDS", "0", "must be from 1 to 32"),
        (
            "IRONCREW_MCP_MAX_REQUEST_STATE_BYTES",
            "1048577",
            "must be from 1 to 1048576",
        ),
        (
            "IRONCREW_MCP_MAX_INBOUND_MESSAGE_BYTES",
            "16777217",
            "must be from 1 to 16777216",
        ),
    ] {
        let error =
            McpCallPolicy::capture_from(|candidate| (candidate == name).then(|| value.to_string()))
                .unwrap_err()
                .to_string();
        assert!(error.contains(expected), "got: {error}");
    }
}

fn policy(values: &[(&str, usize)]) -> McpCallPolicy {
    McpCallPolicy::capture_from(|name| {
        values
            .iter()
            .find(|(candidate, _)| *candidate == name)
            .map(|(_, value)| value.to_string())
    })
    .unwrap()
}

#[allow(dead_code)]
fn assert_value(_: Value) {}
