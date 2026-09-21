use super::*;

/// Parse a non-streaming Responses API response into ChatResponse.
pub(super) fn parse_responses_response(resp: &Value) -> Result<ChatResponse> {
    let output = resp["output"]
        .as_array()
        .ok_or_else(|| IronCrewError::Provider("Missing 'output' array in response".into()))?;

    let mut text_parts: Vec<String> = Vec::new();
    let mut reasoning_parts: Vec<String> = Vec::new();
    let mut tool_calls: Vec<ToolCallRequest> = Vec::new();
    // Reasoning output items captured verbatim (including encrypted_content) so
    // the tool loop can replay them — the Responses API 400s on a function_call
    // sent without its paired reasoning item in stateless mode.
    let mut raw_blocks: Vec<Value> = Vec::new();

    for item in output {
        let item_type = item["type"].as_str().unwrap_or("");
        match item_type {
            "message" => {
                // Collect output_text parts from content array
                if let Some(content) = item["content"].as_array() {
                    for part in content {
                        if part["type"].as_str() == Some("output_text")
                            && let Some(text) = part["text"].as_str()
                        {
                            text_parts.push(text.to_string());
                        }
                    }
                }
            }
            "reasoning" => {
                // Collect summary parts (the full reasoning text isn't exposed)
                if let Some(summary) = item["summary"].as_array() {
                    for s in summary {
                        if let Some(text) = s["text"].as_str() {
                            reasoning_parts.push(text.to_string());
                        }
                    }
                }
                // Keep the whole reasoning item (with encrypted_content) for replay.
                raw_blocks.push(item.clone());
            }
            "function_call" => {
                let call_id = item["call_id"].as_str().unwrap_or("").to_string();
                let name = item["name"].as_str().unwrap_or("").to_string();
                let arguments = item["arguments"].as_str().unwrap_or("{}").to_string();
                tool_calls.push(ToolCallRequest {
                    id: call_id,
                    call_type: "function".to_string(),
                    function: ToolCallFunction { name, arguments },
                });
            }
            "web_search_call" => {
                // Append a summary of the search action
                if let Some(action) = item.get("action") {
                    let query = action["query"].as_str().unwrap_or("");
                    text_parts.push(format!("[Web search: {}]", query));
                }
            }
            "file_search_call" => {
                if let Some(queries) = item["queries"].as_array() {
                    let qs: Vec<&str> = queries.iter().filter_map(|q| q.as_str()).collect();
                    text_parts.push(format!("[File search: {}]", qs.join(", ")));
                }
            }
            "code_interpreter_call" => {
                if let Some(code) = item["code"].as_str() {
                    text_parts.push(format!("[Code executed]\n{}", code));
                }
            }
            _ => {}
        }
    }

    let terminal = matches!(
        resp["status"].as_str(),
        Some("completed" | "failed" | "incomplete" | "cancelled")
    );
    let usage = ProviderUsage::OpenAiResponses.parse(resp.get("usage"), terminal);

    let content = if text_parts.is_empty() {
        None
    } else {
        Some(text_parts.join("\n"))
    };

    let reasoning = if reasoning_parts.is_empty() {
        None
    } else {
        Some(reasoning_parts.join("\n"))
    };

    Ok(ChatResponse {
        content,
        reasoning,
        tool_calls,
        usage,
        raw_blocks: if raw_blocks.is_empty() {
            None
        } else {
            Some(raw_blocks)
        },
    })
}
