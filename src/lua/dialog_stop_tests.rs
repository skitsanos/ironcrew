//! `should_stop` return-value interpretation for `crew:dialog` (split out of
//! dialog.rs to keep the legacy module within its reviewed ceiling).

use super::*;
use crate::llm::provider::{ToolCallFunction, ToolCallRequest};

fn lua() -> mlua::Lua {
    mlua::Lua::new()
}

#[test]
fn nil_means_continue() {
    let result = AgentDialog::interpret_stop_value(mlua::Value::Nil).unwrap();
    assert_eq!(result, None);
}

#[test]
fn false_means_continue() {
    let result = AgentDialog::interpret_stop_value(mlua::Value::Boolean(false)).unwrap();
    assert_eq!(result, None);
}

#[test]
fn true_means_stop_with_default_reason() {
    let result = AgentDialog::interpret_stop_value(mlua::Value::Boolean(true)).unwrap();
    assert_eq!(result.as_deref(), Some("custom_stop"));
}

#[test]
fn string_means_stop_with_that_reason() {
    let lua = lua();
    let s = lua.create_string("consensus reached").unwrap();
    let result = AgentDialog::interpret_stop_value(mlua::Value::String(s)).unwrap();
    assert_eq!(result.as_deref(), Some("consensus reached"));
}

#[test]
fn empty_string_falls_back_to_default_reason() {
    let lua = lua();
    let s = lua.create_string("").unwrap();
    let result = AgentDialog::interpret_stop_value(mlua::Value::String(s)).unwrap();
    assert_eq!(result.as_deref(), Some("custom_stop"));
}

#[test]
fn number_is_rejected_as_usage_error() {
    let result = AgentDialog::interpret_stop_value(mlua::Value::Integer(42));
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("must return nil, bool, or string"),
        "unexpected error: {err}"
    );
}

#[test]
fn table_is_rejected_as_usage_error() {
    let lua = lua();
    let t = lua.create_table().unwrap();
    let result = AgentDialog::interpret_stop_value(mlua::Value::Table(t));
    assert!(result.is_err());
}

#[test]
fn dialog_tool_history_evicts_only_prior_transcript_messages() {
    let tool_call = ToolCallRequest {
        id: "call-1".into(),
        call_type: "function".into(),
        function: ToolCallFunction {
            name: "lookup".into(),
            arguments: "{}".into(),
        },
    };
    let protected = vec![
        ChatMessage::system("system"),
        ChatMessage::user("starter"),
        ChatMessage::assistant(None, Some(vec![tool_call.clone()])),
        ChatMessage::tool("call-1", "result"),
    ];
    let max_bytes = chat_history_estimated_bytes(&protected);
    let mut working = vec![
        ChatMessage::system("system"),
        ChatMessage::user("starter"),
        ChatMessage::user(&"old".repeat(1_000)),
        ChatMessage::assistant(None, Some(vec![tool_call])),
        ChatMessage::tool("call-1", "result"),
    ];

    let active = enforce_dialog_working_history(&mut working, 3, max_bytes).unwrap();

    assert_eq!(active, 2);
    assert_eq!(working.len(), 4);
    assert_eq!(working[2].role, "assistant");
    assert_eq!(working[3].role, "tool");
}

#[test]
fn resumed_dialog_indices_must_match_retained_window() {
    let agents = vec![
        Agent {
            name: "a".into(),
            ..Default::default()
        },
        Agent {
            name: "b".into(),
            ..Default::default()
        },
    ];
    let transcript = VecDeque::from([DialogTurn {
        index: 7,
        speaker_index: 0,
        agent_name: "a".into(),
        content: "hello".into(),
        reasoning: None,
    }]);

    let error = validate_transcript(&transcript, &agents, 10, 10, 1024 * 1024, 7)
        .expect_err("turn index must precede next_index");
    assert!(error.to_string().contains("expected"));
}
