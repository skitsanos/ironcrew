use async_trait::async_trait;
use reqwest::Client;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::time::Duration;

use super::provider::*;
use crate::engine::agent::ResponseFormat;
use crate::utils::error::{IronCrewError, Result};

mod response;
use response::{parse_anthropic_response, structured_output_tool_name};

/// Anthropic-specific configuration (server-side tools, extended thinking).
#[derive(Debug, Clone, Default)]
pub struct AnthropicConfig {
    /// Extended thinking budget in tokens; None = disabled.
    pub thinking_budget: Option<u32>,
    /// Server-side tools to include in every request.
    pub server_tools: Vec<ServerTool>,
}

/// Anthropic server-side tools (executed by Anthropic, not locally).
#[derive(Debug, Clone)]
pub enum ServerTool {
    WebSearch { max_uses: Option<u32> },
    CodeExecution,
}

/// Simple token-bucket rate limiter (same pattern as OpenAI provider).
struct RateLimiter {
    min_interval: Duration,
    last_call: std::sync::Arc<tokio::sync::Mutex<std::time::Instant>>,
}

impl RateLimiter {
    fn new(min_interval_ms: u64) -> Self {
        Self {
            min_interval: Duration::from_millis(min_interval_ms),
            last_call: std::sync::Arc::new(tokio::sync::Mutex::new(
                std::time::Instant::now()
                    .checked_sub(Duration::from_secs(60))
                    .unwrap_or_else(std::time::Instant::now),
            )),
        }
    }

    async fn wait(&self) {
        let mut last = self.last_call.lock().await;
        let elapsed = last.elapsed();
        if elapsed < self.min_interval {
            tokio::time::sleep(self.min_interval - elapsed).await;
        }
        *last = std::time::Instant::now();
    }
}

pub struct AnthropicProvider {
    client: Client,
    base_url: String,
    api_key: String,
    rate_limit: Option<RateLimiter>,
    config: AnthropicConfig,
    execution_policy: super::execution_policy::ProviderExecutionPolicy,
}

impl AnthropicProvider {
    pub fn new(api_key: String, base_url: Option<String>, config: AnthropicConfig) -> Self {
        // Every request carries the API key in `x-api-key`, which reqwest does
        // not strip across hosts, so a redirect must never leave the origin.
        let client = crate::utils::network::secure_client_builder(
            crate::utils::network::OutboundNetworkPolicy::PublicOnly,
        )
        .redirect(crate::utils::network::same_origin_redirect_policy(
            crate::utils::network::OutboundNetworkPolicy::PublicOnly,
            crate::utils::network::private_ips_override_enabled(),
        ))
        .timeout(Duration::from_secs(120))
        .build()
        .expect("Failed to build HTTP client");

        let execution_policy = super::execution_policy::ProviderExecutionPolicy::capture();
        let rate_limit = execution_policy.rate_limit_ms().map(RateLimiter::new);

        Self {
            client,
            base_url: base_url.unwrap_or_else(|| "https://api.anthropic.com".into()),
            api_key,
            rate_limit,
            config,
            execution_policy,
        }
    }

    fn prepare_request(&self, body: &Value) -> Result<Vec<u8>> {
        if self.api_key.trim().is_empty() {
            return Err(IronCrewError::Validation(
                "ANTHROPIC_API_KEY is required for Anthropic provider".into(),
            ));
        }
        self.execution_policy.serialize_request("Anthropic", body)
    }

    /// Build the Anthropic Messages API request body from a ChatRequest.
    fn build_body(&self, request: &ChatRequest, tools: Option<&[ToolSchema]>) -> Value {
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

        // Temperature (forced to 1 when thinking is enabled)
        if self.config.thinking_budget.is_some() {
            // Extended thinking requires temperature = 1 or omitted
        } else if let Some(temp) = request.temperature {
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

        body
    }

    /// Send a non-streaming request to the Anthropic Messages API.
    async fn send_request(
        &self,
        body: Value,
        structured_output_tool: Option<&str>,
    ) -> Result<ChatResponse> {
        let request_body = self.prepare_request(&body)?;

        if let Some(ref limiter) = self.rate_limit {
            limiter.wait().await;
        }

        let url = format!("{}/v1/messages", self.base_url);
        crate::utils::network::validate_url_not_private(&url)
            .map_err(|error| IronCrewError::Provider(format!("Unsafe provider URL: {error}")))?;

        let resp = self
            .client
            .post(&url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .body(request_body)
            .send()
            .await
            .map_err(IronCrewError::Http)?;

        let status = resp.status();
        let response_limit = if status.is_success() {
            self.execution_policy.response_bytes()
        } else {
            self.execution_policy.error_bytes()
        };
        let bytes =
            crate::utils::http::read_response_bytes(resp, response_limit, "Anthropic response")
                .await
                .map_err(|error| IronCrewError::Provider(error.to_string()))?;
        let resp_text = String::from_utf8(bytes).map_err(|_| {
            IronCrewError::Provider("Anthropic response was not valid UTF-8".into())
        })?;
        let resp_body: Value = serde_json::from_str(&resp_text).map_err(|e| {
            tracing::debug!(
                "Raw response: {}",
                crate::utils::http::utf8_prefix(&resp_text, 500)
            );
            IronCrewError::Provider(format!("Invalid JSON from Anthropic: {}", e))
        })?;

        if !status.is_success() {
            let error_msg = resp_body["error"]["message"]
                .as_str()
                .unwrap_or("Unknown Anthropic API error");
            return Err(IronCrewError::Provider(format!(
                "HTTP {}: {}",
                status, error_msg
            )));
        }

        parse_anthropic_response(&resp_body, structured_output_tool)
    }

    /// Send a streaming request to the Anthropic Messages API.
    async fn send_request_stream(
        &self,
        mut body: Value,
        structured_output_tool: Option<&str>,
        tx: tokio::sync::mpsc::Sender<StreamChunk>,
    ) -> Result<ChatResponse> {
        body["stream"] = json!(true);
        let request_body = self.prepare_request(&body)?;

        if let Some(ref limiter) = self.rate_limit {
            limiter.wait().await;
        }

        let url = format!("{}/v1/messages", self.base_url);
        crate::utils::network::validate_url_not_private(&url)
            .map_err(|error| IronCrewError::Provider(format!("Unsafe provider URL: {error}")))?;

        let resp = self
            .client
            .post(&url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .body(request_body)
            .send()
            .await
            .map_err(IronCrewError::Http)?;

        if !resp.status().is_success() {
            let status = resp.status();
            let limit = self.execution_policy.error_bytes();
            let bytes =
                crate::utils::http::read_response_bytes(resp, limit, "Anthropic error response")
                    .await
                    .map_err(|error| IronCrewError::Provider(error.to_string()))?;
            let error_body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
            let error_msg = error_body["error"]["message"]
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| {
                    let raw = String::from_utf8_lossy(&bytes);
                    crate::utils::http::utf8_prefix(raw.trim(), 512).to_owned()
                });
            return Err(IronCrewError::Provider(format!(
                "HTTP {}: {}",
                status, error_msg
            )));
        }

        let mut full_content = String::new();
        let mut full_reasoning = String::new();
        let output_limit = self.execution_policy.output_bytes();
        let mut stored_output_bytes = 0_usize;
        let mut block_states: HashMap<usize, BlockState> = HashMap::new();
        let mut input_tokens: u32 = 0;
        let mut output_tokens: u32 = 0;
        let mut cached_tokens: u32 = 0;

        // Read SSE stream — Anthropic uses `event: <type>\ndata: <json>` format
        let mut stream = resp.bytes_stream();
        use futures::StreamExt;
        let stream_limit = self.execution_policy.stream_bytes();
        let mut buffer =
            crate::utils::http::BoundedLineBuffer::new(stream_limit, "Anthropic stream");
        let mut current_event_type = String::new();
        // Track terminal delivery so a mid-stream `error` event (e.g.
        // `overloaded_error`) or a connection dropped before `message_stop`
        // surfaces as an error instead of a silently-truncated success.
        let mut stream_error: Option<String> = None;
        let mut saw_message_stop = false;

        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(IronCrewError::Http)?;
            let lines = buffer
                .push(&chunk)
                .map_err(|error| IronCrewError::Provider(error.to_string()))?;

            for raw_line in lines {
                let line = raw_line.trim();

                if line.is_empty() {
                    continue;
                }

                // Track event type from `event:` lines
                if let Some(event_type) = line.strip_prefix("event: ") {
                    current_event_type = event_type.trim().to_string();
                    continue;
                }

                // Parse `data:` lines
                let Some(data) = line.strip_prefix("data: ") else {
                    continue;
                };
                let Ok(parsed) = serde_json::from_str::<Value>(data) else {
                    continue;
                };

                match current_event_type.as_str() {
                    "message_start" => {
                        if let Some(usage) = parsed.get("message").and_then(|m| m.get("usage")) {
                            input_tokens = usage["input_tokens"].as_u64().unwrap_or(0) as u32;
                            cached_tokens =
                                usage["cache_read_input_tokens"].as_u64().unwrap_or(0) as u32;
                        }
                    }
                    "content_block_start" => {
                        let index = parsed["index"].as_u64().unwrap_or(0) as usize;
                        let block = &parsed["content_block"];
                        let block_type = block["type"].as_str().unwrap_or("text").to_string();

                        if block_type == "tool_use" {
                            let id = block["id"].as_str().unwrap_or("").to_string();
                            let name = block["name"].as_str().unwrap_or("").to_string();
                            let _ = tx
                                .send(StreamChunk::ToolCallStart {
                                    id: id.clone(),
                                    name: name.clone(),
                                })
                                .await;
                            block_states.insert(
                                index,
                                BlockState {
                                    block_type,
                                    id,
                                    name,
                                    text: String::new(),
                                },
                            );
                        } else {
                            block_states.insert(
                                index,
                                BlockState {
                                    block_type,
                                    id: String::new(),
                                    name: String::new(),
                                    text: String::new(),
                                },
                            );
                        }
                    }
                    "content_block_delta" => {
                        let index = parsed["index"].as_u64().unwrap_or(0) as usize;
                        let delta = &parsed["delta"];
                        let delta_type = delta["type"].as_str().unwrap_or("");

                        match delta_type {
                            "text_delta" => {
                                if let Some(text) = delta["text"].as_str() {
                                    crate::utils::http::bounded_push_str(
                                        &mut full_content,
                                        text,
                                        &mut stored_output_bytes,
                                        output_limit,
                                        "Anthropic accumulated output",
                                    )
                                    .map_err(|error| IronCrewError::Provider(error.to_string()))?;
                                    let _ = tx.send(StreamChunk::Text(text.to_string())).await;
                                    if let Some(state) = block_states.get_mut(&index) {
                                        crate::utils::http::bounded_push_str(
                                            &mut state.text,
                                            text,
                                            &mut stored_output_bytes,
                                            output_limit,
                                            "Anthropic accumulated output",
                                        )
                                        .map_err(
                                            |error| IronCrewError::Provider(error.to_string()),
                                        )?;
                                    }
                                }
                            }
                            "input_json_delta" => {
                                if let Some(partial) = delta["partial_json"].as_str()
                                    && let Some(state) = block_states.get_mut(&index)
                                {
                                    crate::utils::http::bounded_push_str(
                                        &mut state.text,
                                        partial,
                                        &mut stored_output_bytes,
                                        output_limit,
                                        "Anthropic accumulated output",
                                    )
                                    .map_err(|error| IronCrewError::Provider(error.to_string()))?;
                                    let _ = tx
                                        .send(StreamChunk::ToolCallDelta {
                                            id: state.id.clone(),
                                            arguments_delta: partial.to_string(),
                                        })
                                        .await;
                                }
                            }
                            "thinking_delta" => {
                                if let Some(text) = delta["thinking"].as_str() {
                                    crate::utils::http::bounded_push_str(
                                        &mut full_reasoning,
                                        text,
                                        &mut stored_output_bytes,
                                        output_limit,
                                        "Anthropic accumulated output",
                                    )
                                    .map_err(|error| IronCrewError::Provider(error.to_string()))?;
                                    let _ = tx.send(StreamChunk::Thinking(text.to_string())).await;
                                }
                            }
                            _ => {}
                        }
                    }
                    "content_block_stop" => {
                        // Block finalized — state already tracked
                    }
                    "message_delta" => {
                        if let Some(usage) = parsed.get("usage") {
                            output_tokens = usage["output_tokens"].as_u64().unwrap_or(0) as u32;
                        }
                    }
                    "message_stop" => {
                        saw_message_stop = true;
                        let _ = tx.send(StreamChunk::Done).await;
                    }
                    "error" => {
                        // e.g. {"type":"error","error":{"type":"overloaded_error",
                        //       "message":"Overloaded"}}
                        let err = &parsed["error"];
                        let kind = err["type"].as_str().unwrap_or("error");
                        let msg = err["message"].as_str().unwrap_or("stream error");
                        stream_error = Some(format!("{}: {}", kind, msg));
                        break;
                    }
                    _ => {}
                }
            }

            if stream_error.is_some() {
                break;
            }
        }

        // A mid-stream error event, or a stream that ended before `message_stop`,
        // means the response is incomplete — fail rather than return partial text.
        if let Some(err) = stream_error {
            return Err(IronCrewError::Provider(format!(
                "Anthropic stream error — {err}"
            )));
        }
        if !saw_message_stop {
            return Err(IronCrewError::Provider(
                "Anthropic stream ended before message_stop (truncated response)".into(),
            ));
        }

        // Assemble tool calls from block states. A forced structured-output
        // tool carries the answer itself, so its accumulated JSON becomes the
        // response content rather than a tool call.
        let mut structured_output: Option<String> = None;
        let tool_calls: Vec<ToolCallRequest> = block_states
            .into_values()
            .filter(|s| s.block_type == "tool_use" && !s.id.is_empty())
            .filter_map(|s| {
                if structured_output_tool == Some(s.name.as_str()) {
                    structured_output = Some(s.text);
                    return None;
                }
                Some(ToolCallRequest {
                    id: s.id,
                    call_type: "function".to_string(),
                    function: ToolCallFunction {
                        name: s.name,
                        arguments: s.text, // accumulated JSON string
                    },
                })
            })
            .collect();

        if structured_output_tool.is_some() && structured_output.is_none() && tool_calls.is_empty()
        {
            return Err(IronCrewError::Provider(
                "Anthropic response omitted the required structured-output tool".into(),
            ));
        }
        let content = match structured_output {
            Some(json) => Some(json),
            None if full_content.is_empty() => None,
            None => Some(full_content),
        };

        let reasoning = if full_reasoning.is_empty() {
            None
        } else {
            Some(full_reasoning)
        };

        Ok(ChatResponse {
            content,
            reasoning,
            tool_calls,
            usage: Some(TokenUsage {
                prompt_tokens: input_tokens,
                completion_tokens: output_tokens,
                total_tokens: input_tokens + output_tokens,
                cached_tokens,
            }),
            // The streaming path does not reconstruct replayable thinking blocks
            // (that needs the per-block signature reassembled from signature
            // deltas). It isn't required for the tool-use round-trip: the
            // executor forces non-streaming whenever tools are present, and the
            // non-streaming parser above captures the full blocks. If streaming
            // is ever combined with a tool loop, add signature reconstruction
            // here mirroring `parse_anthropic_response`.
            raw_blocks: None,
        })
    }
}

/// State tracked per content block during streaming.
struct BlockState {
    block_type: String,
    id: String,
    name: String,
    text: String, // accumulated text or JSON arguments
}

/// Merge consecutive messages with the same role (Anthropic requires strict alternation).
fn merge_consecutive_roles(messages: Vec<Value>) -> Vec<Value> {
    let mut merged: Vec<Value> = Vec::new();

    for msg in messages {
        let role = msg["role"].as_str().unwrap_or("").to_string();

        if let Some(last) = merged.last_mut()
            && last["role"].as_str() == Some(&role)
        {
            // Merge content blocks
            let existing = last["content"].clone();
            let incoming = msg["content"].clone();

            let mut blocks: Vec<Value> = match existing {
                Value::Array(arr) => arr,
                Value::String(s) => vec![json!({"type": "text", "text": s})],
                _ => Vec::new(),
            };

            match incoming {
                Value::Array(arr) => blocks.extend(arr),
                Value::String(s) => blocks.push(json!({"type": "text", "text": s})),
                _ => {}
            }

            last["content"] = json!(blocks);
            continue;
        }

        merged.push(msg);
    }

    merged
}

#[async_trait]
impl LlmProvider for AnthropicProvider {
    fn metrics_family(&self) -> crate::metrics::ProviderFamily {
        crate::metrics::ProviderFamily::Anthropic
    }

    fn execution_fingerprint(&self) -> Result<String> {
        let server_tools = self
            .config
            .server_tools
            .iter()
            .map(|tool| match tool {
                ServerTool::WebSearch { max_uses } => {
                    json!({"type": "web_search", "max_uses": max_uses})
                }
                ServerTool::CodeExecution => json!({"type": "code_execution"}),
            })
            .collect::<Vec<_>>();
        crate::engine::conversation_provider::provider_execution_fingerprint(
            "anthropic",
            &self.base_url,
            &json!({
                "thinking_budget": self.config.thinking_budget,
                "server_tools": server_tools,
                "execution_policy": self.execution_policy.definition(),
            }),
        )
    }

    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse> {
        tracing::debug!(
            provider = "anthropic",
            model = %request.model,
            messages = request.messages.len(),
            estimated_message_bytes = chat_history_estimated_bytes(&request.messages),
            tools = 0,
            "LLM request metadata"
        );
        let structured_output_tool = structured_output_tool_name(&request);
        let body = self.build_body(&request, None);
        let response = self.send_request(body, structured_output_tool).await?;
        tracing::debug!(
            provider = "anthropic",
            content_bytes = response.content.as_ref().map_or(0, String::len),
            reasoning_bytes = response.reasoning.as_ref().map_or(0, String::len),
            tool_calls = response.tool_calls.len(),
            raw_blocks = response.raw_blocks.as_ref().map_or(0, Vec::len),
            total_tokens = response
                .usage
                .as_ref()
                .map_or(0, |usage| usage.total_tokens),
            "LLM response metadata"
        );
        Ok(response)
    }

    async fn chat_with_tools(
        &self,
        request: ChatRequest,
        tools: &[ToolSchema],
    ) -> Result<ChatResponse> {
        tracing::debug!(
            provider = "anthropic",
            model = %request.model,
            messages = request.messages.len(),
            estimated_message_bytes = chat_history_estimated_bytes(&request.messages),
            tools = tools.len(),
            "LLM request metadata"
        );
        let structured_output_tool = structured_output_tool_name(&request);
        let body = self.build_body(&request, Some(tools));
        let response = self.send_request(body, structured_output_tool).await?;
        tracing::debug!(
            provider = "anthropic",
            content_bytes = response.content.as_ref().map_or(0, String::len),
            reasoning_bytes = response.reasoning.as_ref().map_or(0, String::len),
            tool_calls = response.tool_calls.len(),
            raw_blocks = response.raw_blocks.as_ref().map_or(0, Vec::len),
            total_tokens = response
                .usage
                .as_ref()
                .map_or(0, |usage| usage.total_tokens),
            "LLM response metadata"
        );
        Ok(response)
    }

    async fn chat_stream(
        &self,
        request: ChatRequest,
        tx: tokio::sync::mpsc::Sender<StreamChunk>,
    ) -> Result<ChatResponse> {
        let structured_output_tool = structured_output_tool_name(&request);
        let body = self.build_body(&request, None);
        tracing::debug!("Anthropic streaming request");
        self.send_request_stream(body, structured_output_tool, tx)
            .await
    }
}

#[cfg(test)]
mod tests;
