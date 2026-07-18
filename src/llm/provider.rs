use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::mem::size_of;

use crate::engine::agent::ResponseFormat;
use crate::utils::error::{IronCrewError, Result};

/// Default aggregate in-memory budget for one provider chat history.
pub const DEFAULT_CHAT_HISTORY_MAX_BYTES: usize = 32 * 1024 * 1024;
/// Absolute ceiling for the operator-configurable chat history budget.
pub const HARD_CHAT_HISTORY_MAX_BYTES: usize = 256 * 1024 * 1024;
/// Absolute ceiling for retained non-system messages in a chat history.
pub const HARD_CHAT_HISTORY_MAX_MESSAGES: usize = 4_096;
/// Default retained non-system messages for conversation-style histories.
pub const DEFAULT_CHAT_HISTORY_MAX_MESSAGES: usize = 50;
/// Absolute number of tool calls accepted in one assistant response.
pub const HARD_TOOL_CALLS_PER_ASSISTANT_MESSAGE: usize = 256;
/// Default amount of reasoning text retained across one tool-call loop.
pub const DEFAULT_MAX_REASONING_BYTES: usize = 1024 * 1024;
/// Absolute ceiling for retained reasoning text across one tool-call loop.
pub const HARD_MAX_REASONING_BYTES: usize = 16 * 1024 * 1024;

fn positive_bounded_env(name: &str, default: usize, hard_max: usize) -> usize {
    match std::env::var(name) {
        Ok(raw) => match raw.parse::<usize>() {
            Ok(value) if value > 0 => value.min(hard_max),
            _ => {
                tracing::warn!(
                    env = name,
                    value = %raw,
                    default,
                    "Ignoring invalid positive integer environment value"
                );
                default
            }
        },
        Err(_) => default,
    }
}

/// Resolve the aggregate history budget. Invalid/zero values fail safe to the
/// default and values above the process hard ceiling are clamped.
pub fn chat_history_max_bytes() -> usize {
    positive_bounded_env(
        "IRONCREW_CHAT_HISTORY_MAX_BYTES",
        DEFAULT_CHAT_HISTORY_MAX_BYTES,
        HARD_CHAT_HISTORY_MAX_BYTES,
    )
}

/// Resolve the retained reasoning budget for one provider/tool loop.
pub fn max_reasoning_bytes() -> usize {
    positive_bounded_env(
        "IRONCREW_MAX_REASONING_BYTES",
        DEFAULT_MAX_REASONING_BYTES,
        HARD_MAX_REASONING_BYTES,
    )
}

fn estimated_json_bytes(value: &serde_json::Value) -> usize {
    match value {
        serde_json::Value::Null => 4,
        serde_json::Value::Bool(_) => 5,
        serde_json::Value::Number(_) => 32,
        serde_json::Value::String(value) => value.capacity().saturating_add(size_of::<String>()),
        serde_json::Value::Array(values) => values.iter().fold(
            size_of::<Vec<serde_json::Value>>().saturating_add(
                values
                    .capacity()
                    .saturating_mul(size_of::<serde_json::Value>()),
            ),
            |total, value| total.saturating_add(estimated_json_bytes(value)),
        ),
        serde_json::Value::Object(values) => values.iter().fold(
            size_of::<serde_json::Map<String, serde_json::Value>>(),
            |total, (key, value)| {
                total
                    .saturating_add(size_of::<String>())
                    .saturating_add(key.capacity())
                    .saturating_add(estimated_json_bytes(value))
            },
        ),
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TokenUsage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
    pub cached_tokens: u32,
}

/// An image attachment for a chat message. Always carries base64 data.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageInput {
    pub mime_type: String,
    pub data: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: Option<String>,
    /// For role="tool" messages: the tool_call_id this result corresponds to
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// For role="assistant" messages: tool calls requested by the model
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCallRequest>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub images: Option<Vec<ImageInput>>,
    /// Opaque provider-native blocks captured from the model's response and
    /// replayed verbatim on the next request — Anthropic `thinking`/
    /// `redacted_thinking` blocks (with their signatures) and OpenAI Responses
    /// `reasoning` items. Extended thinking + tools requires these to be echoed
    /// back unchanged; without them the provider rejects the follow-up turn
    /// (Anthropic 400: `tool_use` must be preceded by its `thinking` block).
    /// Ignored by providers that don't need it (OpenAI chat completions).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_blocks: Option<Vec<serde_json::Value>>,
}

impl ChatMessage {
    pub fn system(content: &str) -> Self {
        Self {
            role: "system".into(),
            content: Some(content.into()),
            tool_call_id: None,
            tool_calls: None,
            images: None,
            raw_blocks: None,
        }
    }
    pub fn user(content: &str) -> Self {
        Self {
            role: "user".into(),
            content: Some(content.into()),
            tool_call_id: None,
            tool_calls: None,
            images: None,
            raw_blocks: None,
        }
    }
    pub fn assistant(content: Option<String>, tool_calls: Option<Vec<ToolCallRequest>>) -> Self {
        Self {
            role: "assistant".into(),
            content,
            tool_call_id: None,
            tool_calls,
            images: None,
            raw_blocks: None,
        }
    }
    /// Assistant turn that also carries the provider-native reasoning blocks
    /// (`raw_blocks`) so extended-thinking round-trips replay them verbatim.
    pub fn assistant_with_blocks(
        content: Option<String>,
        tool_calls: Option<Vec<ToolCallRequest>>,
        raw_blocks: Option<Vec<serde_json::Value>>,
    ) -> Self {
        Self {
            role: "assistant".into(),
            content,
            tool_call_id: None,
            tool_calls,
            images: None,
            raw_blocks,
        }
    }
    pub fn tool(tool_call_id: &str, content: &str) -> Self {
        Self {
            role: "tool".into(),
            content: Some(content.into()),
            tool_call_id: Some(tool_call_id.into()),
            tool_calls: None,
            images: None,
            raw_blocks: None,
        }
    }
    pub fn user_with_images(content: &str, images: Vec<ImageInput>) -> Self {
        Self {
            role: "user".into(),
            content: Some(content.into()),
            tool_call_id: None,
            tool_calls: None,
            images: if images.is_empty() {
                None
            } else {
                Some(images)
            },
            raw_blocks: None,
        }
    }

    /// Conservative estimate of the retained memory represented by this
    /// message. It includes every variable-sized field that can be supplied
    /// by a provider or tool, including image base64 and opaque raw blocks.
    pub fn estimated_bytes(&self) -> usize {
        let mut total = size_of::<Self>().saturating_add(self.role.capacity());
        if let Some(content) = &self.content {
            total = total.saturating_add(content.capacity());
        }
        if let Some(tool_call_id) = &self.tool_call_id {
            total = total.saturating_add(tool_call_id.capacity());
        }
        if let Some(tool_calls) = &self.tool_calls {
            total = total.saturating_add(
                tool_calls
                    .capacity()
                    .saturating_mul(size_of::<ToolCallRequest>()),
            );
            for tool_call in tool_calls {
                total = total
                    .saturating_add(tool_call.id.capacity())
                    .saturating_add(tool_call.call_type.capacity())
                    .saturating_add(tool_call.function.name.capacity())
                    .saturating_add(tool_call.function.arguments.capacity());
            }
        }
        if let Some(images) = &self.images {
            total = total.saturating_add(images.capacity().saturating_mul(size_of::<ImageInput>()));
            for image in images {
                total = total
                    .saturating_add(image.mime_type.capacity())
                    .saturating_add(image.data.capacity());
            }
        }
        if let Some(raw_blocks) = &self.raw_blocks {
            total = total.saturating_add(
                raw_blocks
                    .capacity()
                    .saturating_mul(size_of::<serde_json::Value>()),
            );
            for block in raw_blocks {
                total = total.saturating_add(estimated_json_bytes(block));
            }
        }
        total
    }
}

/// Estimate the complete retained footprint of a chat history without
/// serializing it into a second, potentially large temporary allocation.
pub fn chat_history_estimated_bytes(messages: &[ChatMessage]) -> usize {
    messages
        .iter()
        .fold(size_of::<Vec<ChatMessage>>(), |total, message| {
            total.saturating_add(message.estimated_bytes())
        })
}

fn validate_history_protocol(messages: &[ChatMessage], require_complete: bool) -> Result<()> {
    if messages.is_empty() {
        return Err(IronCrewError::Validation(
            "chat history must contain a system message".into(),
        ));
    }
    if messages[0].role != "system" {
        return Err(IronCrewError::Validation(
            "chat history must start with a system message".into(),
        ));
    }

    let mut pending_tool_ids: Option<HashSet<&str>> = None;
    let mut seen_user = false;
    for (index, message) in messages.iter().enumerate() {
        match message.role.as_str() {
            "system" => {
                if index != 0 {
                    return Err(IronCrewError::Validation(format!(
                        "chat history contains a system message at index {index}"
                    )));
                }
                if message.content.is_none() {
                    return Err(IronCrewError::Validation(
                        "system message must contain text".into(),
                    ));
                }
                if message.tool_call_id.is_some()
                    || message.tool_calls.is_some()
                    || message.images.is_some()
                    || message.raw_blocks.is_some()
                {
                    return Err(IronCrewError::Validation(
                        "system message contains fields reserved for another role".into(),
                    ));
                }
            }
            "user" => {
                if pending_tool_ids.as_ref().is_some_and(|ids| !ids.is_empty()) {
                    return Err(IronCrewError::Validation(format!(
                        "chat history starts a user turn at index {index} before all tool results were recorded"
                    )));
                }
                pending_tool_ids = None;
                seen_user = true;
                if message.tool_call_id.is_some()
                    || message.tool_calls.is_some()
                    || message.raw_blocks.is_some()
                {
                    return Err(IronCrewError::Validation(format!(
                        "user message at index {index} contains fields reserved for another role"
                    )));
                }
                if message.content.is_none() && message.images.is_none() {
                    return Err(IronCrewError::Validation(format!(
                        "user message at index {index} has no content"
                    )));
                }
            }
            "assistant" => {
                if !seen_user {
                    return Err(IronCrewError::Validation(format!(
                        "assistant message at index {index} appears before the first user turn"
                    )));
                }
                if pending_tool_ids.as_ref().is_some_and(|ids| !ids.is_empty()) {
                    return Err(IronCrewError::Validation(format!(
                        "chat history contains an assistant message at index {index} before all tool results were recorded"
                    )));
                }
                pending_tool_ids = None;
                if message.tool_call_id.is_some() || message.images.is_some() {
                    return Err(IronCrewError::Validation(format!(
                        "assistant message at index {index} contains fields reserved for another role"
                    )));
                }
                if message.content.is_none()
                    && message.tool_calls.is_none()
                    && message.raw_blocks.is_none()
                {
                    return Err(IronCrewError::Validation(format!(
                        "assistant message at index {index} has no content"
                    )));
                }
                if let Some(tool_calls) = &message.tool_calls {
                    if tool_calls.is_empty() {
                        return Err(IronCrewError::Validation(format!(
                            "assistant message at index {index} has an empty tool_calls list"
                        )));
                    }
                    if tool_calls.len() > HARD_TOOL_CALLS_PER_ASSISTANT_MESSAGE {
                        return Err(IronCrewError::Validation(format!(
                            "assistant message at index {index} contains {} tool calls, exceeding the hard limit of {HARD_TOOL_CALLS_PER_ASSISTANT_MESSAGE}",
                            tool_calls.len()
                        )));
                    }
                    let mut ids = HashSet::with_capacity(tool_calls.len());
                    for call in tool_calls {
                        if call.id.is_empty() || !ids.insert(call.id.as_str()) {
                            return Err(IronCrewError::Validation(format!(
                                "assistant message at index {index} has an empty or duplicate tool call id"
                            )));
                        }
                    }
                    pending_tool_ids = Some(ids);
                }
            }
            "tool" => {
                if message.tool_calls.is_some()
                    || message.images.is_some()
                    || message.raw_blocks.is_some()
                {
                    return Err(IronCrewError::Validation(format!(
                        "tool message at index {index} contains fields reserved for another role"
                    )));
                }
                if message.content.is_none() {
                    return Err(IronCrewError::Validation(format!(
                        "tool message at index {index} has no content"
                    )));
                }
                let Some(tool_call_id) = message.tool_call_id.as_deref() else {
                    return Err(IronCrewError::Validation(format!(
                        "tool message at index {index} is missing tool_call_id"
                    )));
                };
                let Some(ids) = pending_tool_ids.as_mut() else {
                    return Err(IronCrewError::Validation(format!(
                        "orphaned tool message at index {index}"
                    )));
                };
                if !ids.remove(tool_call_id) {
                    return Err(IronCrewError::Validation(format!(
                        "tool message at index {index} has an unknown or duplicate tool_call_id"
                    )));
                }
                if ids.is_empty() {
                    pending_tool_ids = None;
                }
            }
            role => {
                return Err(IronCrewError::Validation(format!(
                    "chat history contains unsupported role '{role}' at index {index}"
                )));
            }
        }
    }

    if require_complete && pending_tool_ids.as_ref().is_some_and(|ids| !ids.is_empty()) {
        return Err(IronCrewError::Validation(
            "chat history ends before all tool results were recorded".into(),
        ));
    }
    Ok(())
}

/// Validate a persisted or provider-bound history before adopting it. The
/// message cap excludes the required system message, matching the public Lua
/// conversation option.
pub fn validate_chat_history(
    messages: &[ChatMessage],
    max_non_system_messages: usize,
    max_bytes: usize,
    require_complete: bool,
) -> Result<()> {
    if max_non_system_messages == 0 || max_non_system_messages > HARD_CHAT_HISTORY_MAX_MESSAGES {
        return Err(IronCrewError::Validation(format!(
            "chat history message cap must be between 1 and {HARD_CHAT_HISTORY_MAX_MESSAGES}"
        )));
    }
    if max_bytes == 0 || max_bytes > HARD_CHAT_HISTORY_MAX_BYTES {
        return Err(IronCrewError::Validation(format!(
            "chat history byte cap must be between 1 and {HARD_CHAT_HISTORY_MAX_BYTES}"
        )));
    }
    let non_system_count = messages.len().saturating_sub(1);
    if non_system_count > max_non_system_messages {
        return Err(IronCrewError::Validation(format!(
            "chat history contains {non_system_count} non-system messages, exceeding the limit of {max_non_system_messages}"
        )));
    }
    let estimated_bytes = chat_history_estimated_bytes(messages);
    if estimated_bytes > max_bytes {
        return Err(IronCrewError::Validation(format!(
            "chat history estimated footprint is {estimated_bytes} bytes, exceeding the limit of {max_bytes} bytes"
        )));
    }
    validate_history_protocol(messages, require_complete)
}

/// Enforce count and aggregate byte limits while retaining the newest complete
/// user turn. Eviction always removes a whole `user .. before-next-user`
/// group, so assistant tool-call requests cannot be separated from their tool
/// results. An oversized current turn is rejected before older groups are
/// removed.
pub fn enforce_conversation_history_limits(
    messages: &mut Vec<ChatMessage>,
    max_non_system_messages: usize,
    max_bytes: usize,
) -> Result<()> {
    if messages.len() < 2 || messages[0].role != "system" {
        return validate_chat_history(messages, max_non_system_messages, max_bytes, false);
    }
    if max_non_system_messages == 0 || max_non_system_messages > HARD_CHAT_HISTORY_MAX_MESSAGES {
        return Err(IronCrewError::Validation(format!(
            "chat history message cap must be between 1 and {HARD_CHAT_HISTORY_MAX_MESSAGES}"
        )));
    }
    if max_bytes == 0 || max_bytes > HARD_CHAT_HISTORY_MAX_BYTES {
        return Err(IronCrewError::Validation(format!(
            "chat history byte cap must be between 1 and {HARD_CHAT_HISTORY_MAX_BYTES}"
        )));
    }
    validate_history_protocol(messages, false)?;

    let active_start = messages
        .iter()
        .rposition(|message| message.role == "user")
        .ok_or_else(|| IronCrewError::Validation("chat history has no user turn".into()))?;
    let protected_count = messages.len().saturating_sub(active_start);
    let protected_bytes = messages[active_start..].iter().fold(
        size_of::<Vec<ChatMessage>>().saturating_add(messages[0].estimated_bytes()),
        |total, message| total.saturating_add(message.estimated_bytes()),
    );
    if protected_count > max_non_system_messages || protected_bytes > max_bytes {
        return Err(IronCrewError::Validation(format!(
            "current chat turn exceeds the configured history budget ({max_non_system_messages} messages / {max_bytes} bytes)"
        )));
    }

    while messages.len().saturating_sub(1) > max_non_system_messages
        || chat_history_estimated_bytes(messages) > max_bytes
    {
        if messages.get(1).is_none_or(|message| message.role != "user") {
            return Err(IronCrewError::Validation(
                "chat history cannot be safely trimmed at a complete user-turn boundary".into(),
            ));
        }
        let next_turn = messages[2..]
            .iter()
            .position(|message| message.role == "user")
            .map(|offset| offset + 2)
            .ok_or_else(|| {
                IronCrewError::Validation(
                    "current chat turn exceeds the configured history budget".into(),
                )
            })?;
        messages.drain(1..next_turn);
    }

    validate_chat_history(messages, max_non_system_messages, max_bytes, false)
}

/// Append UTF-8 text without allowing an accumulator to exceed a byte budget.
/// Returns `true` when any input had to be omitted.
pub fn append_text_bounded(target: &mut String, value: &str, max_bytes: usize) -> bool {
    if value.is_empty() {
        return false;
    }
    let remaining = max_bytes.saturating_sub(target.len());
    if remaining == 0 {
        return true;
    }
    if value.len() <= remaining {
        target.push_str(value);
        return false;
    }
    let boundary = value
        .char_indices()
        .map(|(index, _)| index)
        .take_while(|index| *index <= remaining)
        .last()
        .unwrap_or(0);
    target.push_str(&value[..boundary]);
    true
}

#[derive(Debug, Clone)]
pub struct ChatRequest {
    pub messages: Vec<ChatMessage>,
    pub model: String,
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
    pub response_format: Option<ResponseFormat>,
    pub prompt_cache_key: Option<String>,
    pub prompt_cache_retention: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallRequest {
    pub id: String,
    #[serde(rename = "type")]
    pub call_type: String,
    pub function: ToolCallFunction,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallFunction {
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Clone, Default)]
pub struct ChatResponse {
    pub content: Option<String>,
    /// Accumulated reasoning/thinking text from the model (if any).
    /// Providers: Anthropic (thinking blocks), OpenAI-compat (reasoning_content).
    pub reasoning: Option<String>,
    pub tool_calls: Vec<ToolCallRequest>,
    pub usage: Option<TokenUsage>,
    /// Provider-native reasoning blocks to replay verbatim on the next turn
    /// (see [`ChatMessage::raw_blocks`]). The tool loop copies these onto the
    /// assistant `ChatMessage` it appends to history.
    pub raw_blocks: Option<Vec<serde_json::Value>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolSchema {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

/// A chunk of a streaming response.
#[derive(Debug, Clone)]
#[allow(dead_code)] // variants and fields are used via channel send/receive across modules
pub enum StreamChunk {
    /// A text delta (partial content)
    Text(String),
    /// A reasoning/thinking delta (shown separately from regular output)
    Thinking(String),
    /// A tool call starting
    ToolCallStart { id: String, name: String },
    /// Tool call arguments delta
    ToolCallDelta { id: String, arguments_delta: String },
    /// Stream finished
    Done,
    /// Error during streaming
    Error(String),
}

#[async_trait]
pub trait LlmProvider: Send + Sync {
    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse>;
    async fn chat_with_tools(
        &self,
        request: ChatRequest,
        tools: &[ToolSchema],
    ) -> Result<ChatResponse>;

    /// Stream a chat response. Default implementation falls back to non-streaming.
    async fn chat_stream(
        &self,
        request: ChatRequest,
        tx: tokio::sync::mpsc::Sender<StreamChunk>,
    ) -> Result<ChatResponse> {
        let response = self.chat(request).await?;
        if let Some(ref content) = response.content {
            let _ = tx.send(StreamChunk::Text(content.clone())).await;
        }
        let _ = tx.send(StreamChunk::Done).await;
        Ok(response)
    }
}

#[cfg(test)]
mod history_limit_tests {
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

        let error =
            enforce_conversation_history_limits(&mut history, 10, protected - 1).unwrap_err();

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
}
