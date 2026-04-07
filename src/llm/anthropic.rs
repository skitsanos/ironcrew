use async_trait::async_trait;
use reqwest::Client;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::time::Duration;

use super::provider::*;
use crate::utils::error::{IronCrewError, Result};

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
                std::time::Instant::now() - Duration::from_secs(60),
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
}

impl AnthropicProvider {
    pub fn new(api_key: String, base_url: Option<String>, config: AnthropicConfig) -> Self {
        let client = Client::builder()
            .timeout(Duration::from_secs(120))
            .build()
            .expect("Failed to build HTTP client");

        let rate_limit = std::env::var("IRONCREW_RATE_LIMIT_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|&ms| ms > 0)
            .map(RateLimiter::new);

        Self {
            client,
            base_url: base_url.unwrap_or_else(|| "https://api.anthropic.com".into()),
            api_key,
            rate_limit,
            config,
        }
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
                "user" => json!({
                    "role": "user",
                    "content": msg.content.as_deref().unwrap_or(""),
                }),
                "assistant" => {
                    let mut blocks: Vec<Value> = Vec::new();
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
        let mut body = json!({
            "model": request.model,
            "messages": merged,
            "max_tokens": request.max_tokens.unwrap_or(4096),
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

        if !tools_json.is_empty() {
            body["tools"] = json!(tools_json);
        }

        body
    }

    /// Send a non-streaming request to the Anthropic Messages API.
    async fn send_request(&self, body: Value) -> Result<ChatResponse> {
        if self.api_key.trim().is_empty() {
            return Err(IronCrewError::Validation(
                "ANTHROPIC_API_KEY is required for Anthropic provider".into(),
            ));
        }

        if let Some(ref limiter) = self.rate_limit {
            limiter.wait().await;
        }

        let url = format!("{}/v1/messages", self.base_url);

        let resp = self
            .client
            .post(&url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(IronCrewError::Http)?;

        let status = resp.status();
        let resp_text = resp.text().await.map_err(IronCrewError::Http)?;
        let resp_body: Value = serde_json::from_str(&resp_text).map_err(|e| {
            tracing::debug!("Raw response: {}", &resp_text[..resp_text.len().min(500)]);
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

        parse_anthropic_response(&resp_body)
    }

    /// Send a streaming request to the Anthropic Messages API.
    async fn send_request_stream(
        &self,
        mut body: Value,
        tx: tokio::sync::mpsc::Sender<StreamChunk>,
    ) -> Result<ChatResponse> {
        if self.api_key.trim().is_empty() {
            return Err(IronCrewError::Validation(
                "ANTHROPIC_API_KEY is required for Anthropic provider".into(),
            ));
        }

        if let Some(ref limiter) = self.rate_limit {
            limiter.wait().await;
        }

        body["stream"] = json!(true);

        let url = format!("{}/v1/messages", self.base_url);

        let resp = self
            .client
            .post(&url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(IronCrewError::Http)?;

        if !resp.status().is_success() {
            let status = resp.status();
            let error_body: Value = resp.json().await.map_err(IronCrewError::Http)?;
            let error_msg = error_body["error"]["message"]
                .as_str()
                .unwrap_or("Unknown Anthropic API error");
            return Err(IronCrewError::Provider(format!(
                "HTTP {}: {}",
                status, error_msg
            )));
        }

        let mut full_content = String::new();
        let mut block_states: HashMap<usize, BlockState> = HashMap::new();
        let mut input_tokens: u32 = 0;
        let mut output_tokens: u32 = 0;
        let mut cached_tokens: u32 = 0;

        // Read SSE stream — Anthropic uses `event: <type>\ndata: <json>` format
        let mut stream = resp.bytes_stream();
        use futures::StreamExt;
        let mut buffer = String::new();
        let mut current_event_type = String::new();

        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(IronCrewError::Http)?;
            let text = String::from_utf8_lossy(&chunk);
            buffer.push_str(&text);

            while let Some(newline_pos) = buffer.find('\n') {
                let line = buffer[..newline_pos].trim().to_string();
                buffer = buffer[newline_pos + 1..].to_string();

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
                                    full_content.push_str(text);
                                    let _ = tx.send(StreamChunk::Text(text.to_string())).await;
                                    if let Some(state) = block_states.get_mut(&index) {
                                        state.text.push_str(text);
                                    }
                                }
                            }
                            "input_json_delta" => {
                                if let Some(partial) = delta["partial_json"].as_str()
                                    && let Some(state) = block_states.get_mut(&index)
                                {
                                    state.text.push_str(partial);
                                    let _ = tx
                                        .send(StreamChunk::ToolCallDelta {
                                            id: state.id.clone(),
                                            arguments_delta: partial.to_string(),
                                        })
                                        .await;
                                }
                            }
                            "thinking_delta" => {
                                // Skip thinking deltas — internal reasoning
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
                        let _ = tx.send(StreamChunk::Done).await;
                    }
                    _ => {}
                }
            }
        }

        // Assemble tool calls from block states
        let tool_calls: Vec<ToolCallRequest> = block_states
            .into_values()
            .filter(|s| s.block_type == "tool_use" && !s.id.is_empty())
            .map(|s| ToolCallRequest {
                id: s.id,
                call_type: "function".to_string(),
                function: ToolCallFunction {
                    name: s.name,
                    arguments: s.text, // accumulated JSON string
                },
            })
            .collect();

        let content = if full_content.is_empty() {
            None
        } else {
            Some(full_content)
        };

        Ok(ChatResponse {
            content,
            tool_calls,
            usage: Some(TokenUsage {
                prompt_tokens: input_tokens,
                completion_tokens: output_tokens,
                total_tokens: input_tokens + output_tokens,
                cached_tokens,
            }),
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

/// Parse a non-streaming Anthropic response into ChatResponse.
fn parse_anthropic_response(resp: &Value) -> Result<ChatResponse> {
    let content_blocks = resp["content"]
        .as_array()
        .ok_or_else(|| IronCrewError::Provider("Missing 'content' array in response".into()))?;

    let mut text_parts: Vec<String> = Vec::new();
    let mut tool_calls: Vec<ToolCallRequest> = Vec::new();

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
                // Stringify the input object so the executor can parse it
                let arguments =
                    serde_json::to_string(&block["input"]).unwrap_or_else(|_| "{}".into());
                tool_calls.push(ToolCallRequest {
                    id,
                    call_type: "function".to_string(),
                    function: ToolCallFunction { name, arguments },
                });
            }
            "thinking" => {
                // Skip thinking blocks — internal reasoning
            }
            "web_search_tool_result" => {
                // Append search results as text for the agent to see
                if let Some(content) = block.get("content").and_then(|c| c.as_array()) {
                    for item in content {
                        if item["type"].as_str() == Some("web_search_result") {
                            let title = item["title"].as_str().unwrap_or("");
                            let url = item["url"].as_str().unwrap_or("");
                            let snippets: Vec<&str> = item["content"]
                                .as_array()
                                .map(|arr| arr.iter().filter_map(|s| s["text"].as_str()).collect())
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
                if let Some(stdout) = block.get("content").and_then(|c| {
                    c.as_array().and_then(|arr| {
                        arr.iter()
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

    let usage = resp.get("usage").map(|u| TokenUsage {
        prompt_tokens: u["input_tokens"].as_u64().unwrap_or(0) as u32,
        completion_tokens: u["output_tokens"].as_u64().unwrap_or(0) as u32,
        total_tokens: (u["input_tokens"].as_u64().unwrap_or(0)
            + u["output_tokens"].as_u64().unwrap_or(0)) as u32,
        cached_tokens: u["cache_read_input_tokens"].as_u64().unwrap_or(0) as u32,
    });

    let content = if text_parts.is_empty() {
        None
    } else {
        Some(text_parts.join("\n"))
    };

    Ok(ChatResponse {
        content,
        tool_calls,
        usage,
    })
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
    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse> {
        let body = self.build_body(&request, None);
        tracing::debug!(
            "Anthropic request: {}",
            serde_json::to_string_pretty(&body).unwrap_or_default()
        );
        let response = self.send_request(body).await?;
        tracing::debug!("Anthropic response: {:?}", response);
        Ok(response)
    }

    async fn chat_with_tools(
        &self,
        request: ChatRequest,
        tools: &[ToolSchema],
    ) -> Result<ChatResponse> {
        let body = self.build_body(&request, Some(tools));
        tracing::debug!(
            "Anthropic request (with tools): {}",
            serde_json::to_string_pretty(&body).unwrap_or_default()
        );
        let response = self.send_request(body).await?;
        tracing::debug!("Anthropic response: {:?}", response);
        Ok(response)
    }

    async fn chat_stream(
        &self,
        request: ChatRequest,
        tx: tokio::sync::mpsc::Sender<StreamChunk>,
    ) -> Result<ChatResponse> {
        let body = self.build_body(&request, None);
        tracing::debug!("Anthropic streaming request");
        self.send_request_stream(body, tx).await
    }
}
