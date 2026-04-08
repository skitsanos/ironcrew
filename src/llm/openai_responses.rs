//! OpenAI Responses API provider.
//!
//! Implements the `/v1/responses` endpoint (OpenAI, Azure OpenAI, xAI/Grok,
//! OpenRouter). This endpoint is stateful (via `previous_response_id`) and
//! exposes reasoning items, built-in server-side tools (web_search,
//! file_search, code_interpreter, MCP), and cleaner streaming semantics.
//!
//! This provider treats every task as stateless — it does not chain
//! `previous_response_id`. Full message history is always sent via `input`.

use async_trait::async_trait;
use reqwest::Client;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::time::Duration;

use super::provider::*;
use crate::utils::error::{IronCrewError, Result};

/// OpenAI Responses API-specific configuration.
#[derive(Debug, Clone, Default)]
pub struct ResponsesConfig {
    /// Reasoning effort: "low" | "medium" | "high"
    pub reasoning_effort: Option<String>,
    /// Reasoning summary mode: "auto" | "concise" | "detailed"
    pub reasoning_summary: Option<String>,
    /// Built-in server-side tools to include in every request.
    pub server_tools: Vec<ServerTool>,
}

/// Built-in server-side tools available via Responses API.
#[derive(Debug, Clone)]
#[allow(dead_code)] // Mcp variant is defined but not yet exposed via Lua config
pub enum ServerTool {
    WebSearch {
        context_size: Option<String>,
    },
    FileSearch {
        vector_store_ids: Vec<String>,
        max_num_results: Option<u32>,
    },
    CodeInterpreter,
    Mcp {
        server_label: String,
        server_url: String,
        allowed_tools: Vec<String>,
    },
}

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

pub struct OpenAiResponsesProvider {
    client: Client,
    base_url: String,
    api_key: String,
    rate_limit: Option<RateLimiter>,
    config: ResponsesConfig,
}

impl OpenAiResponsesProvider {
    pub fn new(api_key: String, base_url: Option<String>, config: ResponsesConfig) -> Self {
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
            base_url: base_url.unwrap_or_else(|| "https://api.openai.com".into()),
            api_key,
            rate_limit,
            config,
        }
    }

    /// Detect if the base_url points to xAI/Grok (which doesn't support `instructions` param).
    fn is_grok(&self) -> bool {
        self.base_url.contains("x.ai")
    }

    /// Translate a ChatRequest into a Responses API request body.
    fn build_body(&self, request: &ChatRequest, tools: Option<&[ToolSchema]>) -> Value {
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
                    input_items.push(json!({
                        "type": "message",
                        "role": "user",
                        "content": [{
                            "type": "input_text",
                            "text": msg.content.as_deref().unwrap_or(""),
                        }],
                    }));
                }
                "assistant" => {
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
            body["max_output_tokens"] = json!(max);
        }

        // Temperature
        if let Some(temp) = request.temperature {
            body["temperature"] = json!(temp);
        }

        // Reasoning config
        if self.config.reasoning_effort.is_some() || self.config.reasoning_summary.is_some() {
            let mut reasoning = json!({});
            if let Some(ref effort) = self.config.reasoning_effort {
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

        body
    }

    async fn send_request(&self, body: Value) -> Result<ChatResponse> {
        if self.api_key.trim().is_empty() {
            return Err(IronCrewError::Validation(
                "API key is required for OpenAI Responses provider".into(),
            ));
        }

        if let Some(ref limiter) = self.rate_limit {
            limiter.wait().await;
        }

        let url = format!("{}/v1/responses", self.base_url);

        let resp = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(IronCrewError::Http)?;

        let status = resp.status();
        let resp_text = resp.text().await.map_err(IronCrewError::Http)?;
        let resp_body: Value = serde_json::from_str(&resp_text).map_err(|e| {
            tracing::debug!("Raw response: {}", &resp_text[..resp_text.len().min(500)]);
            IronCrewError::Provider(format!("Invalid JSON from Responses API: {}", e))
        })?;

        if !status.is_success() {
            let error_msg = resp_body["error"]["message"]
                .as_str()
                .unwrap_or("Unknown Responses API error");
            return Err(IronCrewError::Provider(format!(
                "HTTP {}: {}",
                status, error_msg
            )));
        }

        parse_responses_response(&resp_body)
    }

    async fn send_request_stream(
        &self,
        mut body: Value,
        tx: tokio::sync::mpsc::Sender<StreamChunk>,
    ) -> Result<ChatResponse> {
        if self.api_key.trim().is_empty() {
            return Err(IronCrewError::Validation(
                "API key is required for OpenAI Responses provider".into(),
            ));
        }

        if let Some(ref limiter) = self.rate_limit {
            limiter.wait().await;
        }

        body["stream"] = json!(true);

        let url = format!("{}/v1/responses", self.base_url);

        let resp = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(IronCrewError::Http)?;

        if !resp.status().is_success() {
            let status = resp.status();
            let error_body: Value = resp.json().await.map_err(IronCrewError::Http)?;
            let error_msg = error_body["error"]["message"]
                .as_str()
                .unwrap_or("Unknown Responses API error");
            return Err(IronCrewError::Provider(format!(
                "HTTP {}: {}",
                status, error_msg
            )));
        }

        let mut full_content = String::new();
        let mut full_reasoning = String::new();
        let mut item_states: HashMap<String, ItemState> = HashMap::new();
        let mut usage_data: Option<Value> = None;

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

                if let Some(event_type) = line.strip_prefix("event: ") {
                    current_event_type = event_type.trim().to_string();
                    continue;
                }

                let Some(data) = line.strip_prefix("data: ") else {
                    continue;
                };

                if data == "[DONE]" {
                    let _ = tx.send(StreamChunk::Done).await;
                    continue;
                }

                let Ok(parsed) = serde_json::from_str::<Value>(data) else {
                    continue;
                };

                match current_event_type.as_str() {
                    "response.output_item.added" => {
                        let item = &parsed["item"];
                        let item_id = item["id"].as_str().unwrap_or("").to_string();
                        let item_type = item["type"].as_str().unwrap_or("").to_string();

                        if item_type == "function_call" {
                            let name = item["name"].as_str().unwrap_or("").to_string();
                            let call_id =
                                item["call_id"].as_str().unwrap_or("").to_string();
                            let _ = tx
                                .send(StreamChunk::ToolCallStart {
                                    id: call_id.clone(),
                                    name: name.clone(),
                                })
                                .await;
                            item_states.insert(
                                item_id.clone(),
                                ItemState {
                                    item_type,
                                    call_id,
                                    name,
                                    text: String::new(),
                                },
                            );
                        } else {
                            item_states.insert(
                                item_id,
                                ItemState {
                                    item_type,
                                    call_id: String::new(),
                                    name: String::new(),
                                    text: String::new(),
                                },
                            );
                        }
                    }
                    "response.output_text.delta" => {
                        if let Some(delta) = parsed["delta"].as_str() {
                            full_content.push_str(delta);
                            let _ = tx.send(StreamChunk::Text(delta.to_string())).await;
                        }
                    }
                    "response.reasoning_summary_text.delta"
                    | "response.reasoning_text.delta" => {
                        if let Some(delta) = parsed["delta"].as_str() {
                            full_reasoning.push_str(delta);
                            let _ = tx.send(StreamChunk::Thinking(delta.to_string())).await;
                        }
                    }
                    "response.function_call_arguments.delta" => {
                        if let Some(delta) = parsed["delta"].as_str() {
                            let item_id = parsed["item_id"].as_str().unwrap_or("");
                            if let Some(state) = item_states.get_mut(item_id) {
                                state.text.push_str(delta);
                                let _ = tx
                                    .send(StreamChunk::ToolCallDelta {
                                        id: state.call_id.clone(),
                                        arguments_delta: delta.to_string(),
                                    })
                                    .await;
                            }
                        }
                    }
                    "response.completed" => {
                        if let Some(usage) = parsed["response"].get("usage").cloned() {
                            usage_data = Some(usage);
                        }
                        let _ = tx.send(StreamChunk::Done).await;
                    }
                    "response.failed" | "error" => {
                        let err_msg = parsed["error"]["message"]
                            .as_str()
                            .or_else(|| parsed["response"]["error"]["message"].as_str())
                            .unwrap_or("Responses API stream error");
                        let _ = tx.send(StreamChunk::Error(err_msg.to_string())).await;
                    }
                    _ => {}
                }
            }
        }

        // Assemble tool calls from item states
        let tool_calls: Vec<ToolCallRequest> = item_states
            .into_values()
            .filter(|s| s.item_type == "function_call" && !s.call_id.is_empty())
            .map(|s| ToolCallRequest {
                id: s.call_id,
                call_type: "function".to_string(),
                function: ToolCallFunction {
                    name: s.name,
                    arguments: s.text,
                },
            })
            .collect();

        let content = if full_content.is_empty() {
            None
        } else {
            Some(full_content)
        };

        let reasoning = if full_reasoning.is_empty() {
            None
        } else {
            Some(full_reasoning)
        };

        let usage = usage_data.map(|u| TokenUsage {
            prompt_tokens: u["input_tokens"].as_u64().unwrap_or(0) as u32,
            completion_tokens: u["output_tokens"].as_u64().unwrap_or(0) as u32,
            total_tokens: u["total_tokens"].as_u64().unwrap_or(0) as u32,
            cached_tokens: u["input_tokens_details"]["cached_tokens"]
                .as_u64()
                .unwrap_or(0) as u32,
        });

        Ok(ChatResponse {
            content,
            reasoning,
            tool_calls,
            usage,
        })
    }
}

/// State tracked per output item during streaming.
struct ItemState {
    item_type: String,
    call_id: String,
    name: String,
    text: String,
}

/// Parse a non-streaming Responses API response into ChatResponse.
fn parse_responses_response(resp: &Value) -> Result<ChatResponse> {
    let output = resp["output"]
        .as_array()
        .ok_or_else(|| IronCrewError::Provider("Missing 'output' array in response".into()))?;

    let mut text_parts: Vec<String> = Vec::new();
    let mut reasoning_parts: Vec<String> = Vec::new();
    let mut tool_calls: Vec<ToolCallRequest> = Vec::new();

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

    let usage = resp.get("usage").map(|u| TokenUsage {
        prompt_tokens: u["input_tokens"].as_u64().unwrap_or(0) as u32,
        completion_tokens: u["output_tokens"].as_u64().unwrap_or(0) as u32,
        total_tokens: u["total_tokens"].as_u64().unwrap_or(0) as u32,
        cached_tokens: u["input_tokens_details"]["cached_tokens"]
            .as_u64()
            .unwrap_or(0) as u32,
    });

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
    })
}

#[async_trait]
impl LlmProvider for OpenAiResponsesProvider {
    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse> {
        let body = self.build_body(&request, None);
        tracing::debug!(
            "Responses API request: {}",
            serde_json::to_string_pretty(&body).unwrap_or_default()
        );
        let response = self.send_request(body).await?;
        tracing::debug!("Responses API response: {:?}", response);
        Ok(response)
    }

    async fn chat_with_tools(
        &self,
        request: ChatRequest,
        tools: &[ToolSchema],
    ) -> Result<ChatResponse> {
        let body = self.build_body(&request, Some(tools));
        tracing::debug!(
            "Responses API request (with tools): {}",
            serde_json::to_string_pretty(&body).unwrap_or_default()
        );
        let response = self.send_request(body).await?;
        tracing::debug!("Responses API response: {:?}", response);
        Ok(response)
    }

    async fn chat_stream(
        &self,
        request: ChatRequest,
        tx: tokio::sync::mpsc::Sender<StreamChunk>,
    ) -> Result<ChatResponse> {
        let body = self.build_body(&request, None);
        tracing::debug!("Responses API streaming request");
        self.send_request_stream(body, tx).await
    }
}
