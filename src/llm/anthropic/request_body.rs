use super::*;
use crate::engine::agent::ResponseFormat;

impl AnthropicProvider {
    pub(super) fn build_body(
        &self,
        request: &ChatRequest,
        tools: Option<&[ToolSchema]>,
    ) -> Result<Value> {
        self.validate_request(request, tools.is_some_and(|tools| !tools.is_empty()))?;
        // 1. Extract system messages → top-level `system` param
        let system_parts: Vec<&str> = request
            .messages
            .iter()
            .filter(|m| m.role == "system")
            .filter_map(|m| m.content.as_deref())
            .collect();

        // 2. Translate non-system messages to Anthropic format
        let mut anthropic_messages: Vec<Value> = Vec::new();

        for msg in &request.messages {
            if msg.role == "system" {
                continue;
            }

            let translated = match msg.role.as_str() {
                "user" => {
                    if let Some(ref images) = msg.images {
                        if !images.is_empty() {
                            let mut parts: Vec<serde_json::Value> = Vec::new();
                            // Anthropic recommends images before text
                            for img in images {
                                parts.push(json!({
                                    "type": "image",
                                    "source": {
                                        "type": "base64",
                                        "media_type": img.mime_type,
                                        "data": img.data,
                                    }
                                }));
                            }
                            if let Some(ref text) = msg.content {
                                parts.push(json!({"type": "text", "text": text}));
                            }
                            json!({"role": "user", "content": parts})
                        } else {
                            json!({
                                "role": "user",
                                "content": msg.content.as_deref().unwrap_or(""),
                            })
                        }
                    } else {
                        json!({
                            "role": "user",
                            "content": msg.content.as_deref().unwrap_or(""),
                        })
                    }
                }
                "assistant" => {
                    let mut blocks: Vec<Value> = Vec::new();
                    // Replay captured thinking/redacted_thinking blocks FIRST and
                    // verbatim (signatures intact). With extended thinking + tools,
                    // Anthropic requires the thinking block to precede tool_use;
                    // omitting or modifying it returns a 400.
                    if let Some(ref raw) = msg.raw_blocks {
                        blocks.extend(raw.iter().cloned());
                    }
                    if let Some(ref content) = msg.content
                        && !content.is_empty()
                    {
                        blocks.push(json!({"type": "text", "text": content}));
                    }
                    // Convert tool_calls to tool_use content blocks
                    if let Some(ref tool_calls) = msg.tool_calls {
                        for tc in tool_calls {
                            let input: Value =
                                serde_json::from_str(&tc.function.arguments).unwrap_or(json!({}));
                            blocks.push(json!({
                                "type": "tool_use",
                                "id": tc.id,
                                "name": tc.function.name,
                                "input": input,
                            }));
                        }
                    }
                    if blocks.is_empty() {
                        blocks.push(json!({"type": "text", "text": ""}));
                    }
                    json!({"role": "assistant", "content": blocks})
                }
                "tool" => {
                    // Tool results become user messages with tool_result content blocks
                    json!({
                        "role": "user",
                        "content": [{
                            "type": "tool_result",
                            "tool_use_id": msg.tool_call_id.as_deref().unwrap_or(""),
                            "content": msg.content.as_deref().unwrap_or(""),
                        }]
                    })
                }
                _ => continue,
            };

            anthropic_messages.push(translated);
        }

        // 3. Merge consecutive same-role messages (Anthropic requires strict alternation)
        let merged = merge_consecutive_roles(anthropic_messages);

        // 4. Build request body
        // When thinking is enabled, max_tokens must exceed the thinking budget
        let default_max_tokens = match self.config.thinking_budget {
            Some(budget) => budget + 4096, // budget + room for the actual response
            None => 4096,
        };
        let mut body = json!({
            "model": request.model,
            "messages": merged,
            "max_tokens": request.max_tokens.unwrap_or(default_max_tokens),
        });

        // System prompt
        if !system_parts.is_empty() {
            let system_text = system_parts.join("\n\n");
            if request.prompt_cache_key.is_some() {
                // Use content blocks with cache_control for prompt caching
                body["system"] = json!([{
                    "type": "text",
                    "text": system_text,
                    "cache_control": {"type": "ephemeral"},
                }]);
            } else {
                body["system"] = json!(system_text);
            }
        }

        // Explicit supported settings are preserved; unsupported values fail above.
        if let Some(temp) = request.temperature {
            body["temperature"] = json!(temp);
        }

        // Extended thinking
        if let Some(budget) = self.config.thinking_budget {
            body["thinking"] = json!({
                "type": "enabled",
                "budget_tokens": budget,
            });
        }

        // 5. Map user-defined tools
        let mut tools_json: Vec<Value> = Vec::new();
        if let Some(tool_schemas) = tools {
            for t in tool_schemas {
                tools_json.push(json!({
                    "name": t.name,
                    "description": t.description,
                    "input_schema": t.parameters,
                }));
            }
        }

        // 6. Append server-side tools
        for st in &self.config.server_tools {
            match st {
                ServerTool::WebSearch { max_uses } => {
                    let mut tool = json!({
                        "type": "web_search_20250305",
                        "name": "web_search",
                    });
                    if let Some(max) = max_uses {
                        tool["max_uses"] = json!(max);
                    }
                    tools_json.push(tool);
                }
                ServerTool::CodeExecution => {
                    tools_json.push(json!({
                        "type": "code_execution_20250522",
                        "name": "code_execution",
                    }));
                }
            }
        }

        // 7. Structured output. The Messages API has no `response_format`, so a
        // JSON Schema is enforced by defining a single-purpose tool and forcing
        // the model to call it. `JsonObject`/`Text` have no schema to bind and
        // are steered through the system prompt instead (see `build_system`).
        let has_other_tools = !tools_json.is_empty();
        let schema_tool = match request.response_format {
            Some(ResponseFormat::JsonSchema {
                ref name,
                ref schema,
            }) => {
                tools_json.push(json!({
                    "name": name,
                    "description":
                        "Return the final answer. You must call this tool exactly once \
                         with the complete result.",
                    "input_schema": schema,
                }));
                Some(name.clone())
            }
            _ => None,
        };

        if !tools_json.is_empty() {
            body["tools"] = json!(tools_json);
        }

        // Force the structured-output tool so the model cannot answer in prose.
        if let Some(name) = schema_tool
            && !has_other_tools
        {
            body["tool_choice"] = json!({"type": "tool", "name": name});
        }

        Ok(body)
    }
}
