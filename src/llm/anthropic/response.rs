use serde_json::Value;

use crate::engine::agent::ResponseFormat;
use crate::llm::provider::{
    ChatRequest, ChatResponse, TokenUsage, ToolCallFunction, ToolCallRequest,
};
use crate::utils::error::{IronCrewError, Result};

pub(super) fn structured_output_tool_name(request: &ChatRequest) -> Option<&str> {
    match request.response_format.as_ref() {
        Some(ResponseFormat::JsonSchema { name, .. }) => Some(name),
        _ => None,
    }
}

pub(super) fn parse_anthropic_response(
    resp: &Value,
    structured_output_tool: Option<&str>,
) -> Result<ChatResponse> {
    let content_blocks = resp["content"]
        .as_array()
        .ok_or_else(|| IronCrewError::Provider("Missing 'content' array in response".into()))?;

    let mut text_parts: Vec<String> = Vec::new();
    let mut reasoning_parts: Vec<String> = Vec::new();
    let mut tool_calls: Vec<ToolCallRequest> = Vec::new();
    let mut saw_structured_output = false;
    // Thinking blocks are captured verbatim so Anthropic can validate their
    // signatures when the executor replays a tool-use round.
    let mut raw_blocks: Vec<Value> = Vec::new();

    for block in content_blocks {
        let block_type = block["type"].as_str().unwrap_or("");
        match block_type {
            "text" => {
                if let Some(text) = block["text"].as_str() {
                    text_parts.push(text.to_string());
                }
            }
            "tool_use" => {
                let id = block["id"].as_str().unwrap_or("").to_string();
                let name = block["name"].as_str().unwrap_or("").to_string();
                let arguments =
                    serde_json::to_string(&block["input"]).unwrap_or_else(|_| "{}".into());
                if structured_output_tool == Some(name.as_str()) {
                    saw_structured_output = true;
                    text_parts.push(arguments);
                    continue;
                }
                tool_calls.push(ToolCallRequest {
                    id,
                    call_type: "function".to_string(),
                    function: ToolCallFunction { name, arguments },
                });
            }
            "thinking" => {
                if let Some(text) = block["thinking"].as_str() {
                    reasoning_parts.push(text.to_string());
                }
                raw_blocks.push(block.clone());
            }
            "redacted_thinking" => raw_blocks.push(block.clone()),
            "web_search_tool_result" => {
                if let Some(content) = block.get("content").and_then(|value| value.as_array()) {
                    for item in content {
                        if item["type"].as_str() == Some("web_search_result") {
                            let title = item["title"].as_str().unwrap_or("");
                            let url = item["url"].as_str().unwrap_or("");
                            let snippets: Vec<&str> = item["content"]
                                .as_array()
                                .map(|items| {
                                    items
                                        .iter()
                                        .filter_map(|snippet| snippet["text"].as_str())
                                        .collect()
                                })
                                .unwrap_or_default();
                            text_parts.push(format!(
                                "[Web: {} ({})] {}",
                                title,
                                url,
                                snippets.join(" ")
                            ));
                        }
                    }
                }
            }
            "code_execution_tool_result" => {
                if let Some(stdout) = block.get("content").and_then(|content| {
                    content.as_array().and_then(|items| {
                        items
                            .iter()
                            .find(|item| item["type"].as_str() == Some("output"))
                            .and_then(|item| item["output"].as_str())
                    })
                }) {
                    text_parts.push(format!("[Code output] {}", stdout));
                }
            }
            _ => {}
        }
    }

    let usage = resp.get("usage").map(|usage| TokenUsage {
        prompt_tokens: usage["input_tokens"].as_u64().unwrap_or(0) as u32,
        completion_tokens: usage["output_tokens"].as_u64().unwrap_or(0) as u32,
        total_tokens: (usage["input_tokens"].as_u64().unwrap_or(0)
            + usage["output_tokens"].as_u64().unwrap_or(0)) as u32,
        cached_tokens: usage["cache_read_input_tokens"].as_u64().unwrap_or(0) as u32,
    });

    if structured_output_tool.is_some() && !saw_structured_output && tool_calls.is_empty() {
        return Err(IronCrewError::Provider(
            "Anthropic response omitted the required structured-output tool".into(),
        ));
    }

    Ok(ChatResponse {
        content: (!text_parts.is_empty()).then(|| text_parts.join("\n")),
        reasoning: (!reasoning_parts.is_empty()).then(|| reasoning_parts.join("\n")),
        tool_calls,
        usage,
        raw_blocks: (!raw_blocks.is_empty()).then_some(raw_blocks),
    })
}
