use super::*;
use serde_json::json;

fn tool_request(id: &str, arguments: &str) -> ToolCallRequest {
    ToolCallRequest {
        id: id.into(),
        call_type: "function".into(),
        function: ToolCallFunction {
            name: "lookup".into(),
            arguments: arguments.into(),
        },
    }
}

#[test]
fn footprint_includes_images_tool_arguments_ids_and_raw_blocks() {
    let baseline = ChatMessage::assistant(None, None).estimated_bytes();
    let message = ChatMessage {
        role: "assistant".into(),
        content: Some("content".into()),
        tool_call_id: Some("tool-id".into()),
        tool_calls: Some(vec![tool_request("call-id", &"x".repeat(1_000))]),
        images: Some(vec![ImageInput {
            mime_type: "image/png".into(),
            data: "a".repeat(2_000),
        }]),
        raw_blocks: Some(vec![json!({"reasoning": "r".repeat(3_000)})]),
    };
    assert!(message.estimated_bytes() >= baseline + 6_000);
}

#[test]
fn eviction_removes_an_entire_tool_call_turn() {
    let mut history = vec![
        ChatMessage::system("system"),
        ChatMessage::user("old"),
        ChatMessage::assistant(None, Some(vec![tool_request("old-call", "{}")])),
        ChatMessage::tool("old-call", "old-result"),
        ChatMessage::assistant(Some("old-answer".into()), None),
        ChatMessage::user("current"),
        ChatMessage::assistant(Some("current-answer".into()), None),
    ];

    enforce_conversation_history_limits(&mut history, 2, 1024 * 1024).unwrap();

    assert_eq!(history.len(), 3);
    assert_eq!(history[1].content.as_deref(), Some("current"));
    assert_eq!(history[2].content.as_deref(), Some("current-answer"));
    assert!(history.iter().all(|message| {
        message.tool_call_id.as_deref() != Some("old-call")
            && message
                .tool_calls
                .as_ref()
                .is_none_or(|calls| calls.iter().all(|call| call.id != "old-call"))
    }));
}

#[test]
fn oversized_active_turn_fails_before_evicting_prior_turns() {
    let mut history = vec![
        ChatMessage::system("system"),
        ChatMessage::user("old"),
        ChatMessage::assistant(Some("answer".into()), None),
        ChatMessage::user(&"x".repeat(4_096)),
    ];
    let original_roles: Vec<String> = history.iter().map(|m| m.role.clone()).collect();
    let protected = history[0]
        .estimated_bytes()
        .saturating_add(history[3].estimated_bytes());

    let error = enforce_conversation_history_limits(&mut history, 10, protected - 1).unwrap_err();

    assert!(error.to_string().contains("current chat turn"));
    assert_eq!(
        history.iter().map(|m| m.role.clone()).collect::<Vec<_>>(),
        original_roles
    );
}

#[test]
fn persisted_orphan_tool_result_is_rejected() {
    let history = vec![
        ChatMessage::system("system"),
        ChatMessage::user("question"),
        ChatMessage::tool("missing-call", "result"),
    ];
    let error = validate_chat_history(&history, 50, 1024 * 1024, true).unwrap_err();
    assert!(error.to_string().contains("orphaned tool message"));
}

#[test]
fn one_response_cannot_schedule_unbounded_tool_calls() {
    let calls = (0..=HARD_TOOL_CALLS_PER_ASSISTANT_MESSAGE)
        .map(|index| tool_request(&format!("call-{index}"), "{}"))
        .collect();
    let history = vec![
        ChatMessage::system("system"),
        ChatMessage::user("question"),
        ChatMessage::assistant(None, Some(calls)),
    ];
    let error = validate_chat_history(&history, 50, 1024 * 1024, false).unwrap_err();
    assert!(error.to_string().contains("tool calls"));
}

#[test]
fn bounded_append_never_splits_utf8() {
    let mut output = String::new();
    assert!(append_text_bounded(&mut output, "🦀🦀", 5));
    assert_eq!(output, "🦀");
    assert!(output.len() <= 5);
}
