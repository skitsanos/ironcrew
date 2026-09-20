use super::*;
use crate::engine::agent::ResponseFormat;

impl OpenAiResponsesProvider {
    pub(super) fn build_body(
        &self,
        request: &ChatRequest,
        tools: Option<&[ToolSchema]>,
    ) -> Result<Value> {
        let options =
            self.resolve_options(request, tools.is_some_and(|tools| !tools.is_empty()))?;
        // 1. Extract system messages → instructions param
        let instructions_text: Vec<&str> = request
            .messages
            .iter()
            .filter(|m| m.role == "system")
            .filter_map(|m| m.content.as_deref())
            .collect();

        // 2. Build input array from non-system messages
        let mut input_items: Vec<Value> = Vec::new();

        // If Grok, inject system as a user-role message at the start
        if self.is_grok() && !instructions_text.is_empty() {
            input_items.push(json!({
                "type": "message",
                "role": "system",
                "content": [{
                    "type": "input_text",
                    "text": instructions_text.join("\n\n"),
                }],
            }));
        }

        for msg in &request.messages {
            if msg.role == "system" {
                continue;
            }

            match msg.role.as_str() {
                "user" => {
                    // Text first, then any attachments as `input_image` parts.
                    // Dropping images here would silently answer as if nothing
                    // had been attached.
                    let mut content = vec![json!({
                        "type": "input_text",
                        "text": msg.content.as_deref().unwrap_or(""),
                    })];
                    if let Some(ref images) = msg.images {
                        for image in images {
                            content.push(json!({
                                "type": "input_image",
                                "image_url": format!(
                                    "data:{};base64,{}",
                                    image.mime_type, image.data
                                ),
                            }));
                        }
                    }
                    input_items.push(json!({
                        "type": "message",
                        "role": "user",
                        "content": content,
                    }));
                }
                "assistant" => {
                    // Replay captured `reasoning` output items verbatim, before
                    // the text/function_call items. The Responses API 400s in
                    // stateless mode (store:false) when a `function_call` is sent
                    // without its paired reasoning item for reasoning models.
                    if let Some(ref raw) = msg.raw_blocks {
                        for item in raw {
                            input_items.push(item.clone());
                        }
                    }
                    // Text portion (if any)
                    if let Some(ref content) = msg.content
                        && !content.is_empty()
                    {
                        input_items.push(json!({
                            "type": "message",
                            "role": "assistant",
                            "content": [{
                                "type": "output_text",
                                "text": content,
                            }],
                        }));
                    }
                    // Tool calls become separate function_call items
                    if let Some(ref tool_calls) = msg.tool_calls {
                        for tc in tool_calls {
                            input_items.push(json!({
                                "type": "function_call",
                                "call_id": tc.id,
                                "name": tc.function.name,
                                "arguments": tc.function.arguments,
                            }));
                        }
                    }
                }
                "tool" => {
                    // Tool results become top-level function_call_output items
                    input_items.push(json!({
                        "type": "function_call_output",
                        "call_id": msg.tool_call_id.as_deref().unwrap_or(""),
                        "output": msg.content.as_deref().unwrap_or(""),
                    }));
                }
                _ => continue,
            }
        }

        let mut body = json!({
            "model": request.model,
            "input": input_items,
            "store": false,
        });

        // Instructions (non-Grok providers)
        if !self.is_grok() && !instructions_text.is_empty() {
            body["instructions"] = json!(instructions_text.join("\n\n"));
        }

        // Max output tokens
        if let Some(max) = request.max_tokens {
            body[options.token_field] = json!(max);
        }

        // Temperature
        if let Some(temp) = request.temperature {
            body["temperature"] = json!(temp);
        }

        // Structured output. The Responses API nests the format under `text`
        // rather than using Chat Completions' top-level `response_format`.
        if let Some(ref format) = request.response_format {
            body["text"] = match format {
                ResponseFormat::Text => json!({"format": {"type": "text"}}),
                ResponseFormat::JsonObject => json!({"format": {"type": "json_object"}}),
                ResponseFormat::JsonSchema { name, schema } => json!({
                    "format": {
                        "type": "json_schema",
                        "name": name,
                        "schema": schema,
                        "strict": true,
                    }
                }),
            };
        }

        // Reasoning config
        // Per-agent effort (request) wins over the crew-level config.
        let reasoning_effort = options.effort;
        if reasoning_effort.is_some() || self.config.reasoning_summary.is_some() {
            let mut reasoning = json!({});
            if let Some(effort) = reasoning_effort {
                reasoning["effort"] = json!(effort);
            }
            if let Some(ref summary) = self.config.reasoning_summary {
                reasoning["summary"] = json!(summary);
            }
            body["reasoning"] = reasoning;

            // Include encrypted reasoning content for stateless multi-turn
            body["include"] = json!(["reasoning.encrypted_content"]);
        }

        // 3. Build tools array
        let mut tools_json: Vec<Value> = Vec::new();

        // Custom function tools
        if let Some(tool_schemas) = tools {
            for t in tool_schemas {
                tools_json.push(json!({
                    "type": "function",
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.parameters,
                    "strict": true,
                }));
            }
        }

        // Server-side tools
        for st in &self.config.server_tools {
            match st {
                ServerTool::WebSearch { context_size } => {
                    let mut tool = json!({"type": "web_search"});
                    if let Some(cs) = context_size {
                        tool["search_context_size"] = json!(cs);
                    }
                    tools_json.push(tool);
                }
                ServerTool::FileSearch {
                    vector_store_ids,
                    max_num_results,
                } => {
                    let mut tool = json!({
                        "type": "file_search",
                        "vector_store_ids": vector_store_ids,
                    });
                    if let Some(max) = max_num_results {
                        tool["max_num_results"] = json!(max);
                    }
                    tools_json.push(tool);
                }
                ServerTool::CodeInterpreter => {
                    tools_json.push(json!({
                        "type": "code_interpreter",
                        "container": {"type": "auto"},
                    }));
                }
                ServerTool::Mcp {
                    server_label,
                    server_url,
                    allowed_tools,
                } => {
                    tools_json.push(json!({
                        "type": "mcp",
                        "server_label": server_label,
                        "server_url": server_url,
                        "allowed_tools": allowed_tools,
                        "require_approval": "never",
                    }));
                }
            }
        }

        if !tools_json.is_empty() {
            body["tools"] = json!(tools_json);
        }

        Ok(body)
    }

    pub(super) fn resolve_options<'a>(
        &'a self,
        request: &'a ChatRequest,
        has_tools: bool,
    ) -> Result<super::super::capabilities::Resolved<'a>> {
        super::super::capabilities::openai(
            super::super::capabilities::Transport::Responses,
            &self.base_url,
            request,
            has_tools || !self.config.server_tools.is_empty(),
            self.config.reasoning_effort.as_deref(),
        )
    }
}
