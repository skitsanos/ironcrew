use async_trait::async_trait;
use reqwest::Client;
use serde_json::{Value, json};

use super::provider::*;
use super::provider_http::{ProviderSseLines, RateLimiter, read_error_response, sse_field};
use crate::engine::agent::ResponseFormat;
use crate::utils::error::{IronCrewError, Result};

mod request_body;
mod stream_tools;

pub struct OpenAiProvider {
    client: Client,
    base_url: String,
    api_key: String,
    rate_limit: Option<RateLimiter>,
    execution_policy: super::execution_policy::ProviderExecutionPolicy,
}

impl OpenAiProvider {
    pub fn new(api_key: String, base_url: Option<String>) -> Self {
        // Capture provider policy once so durable conversation identity and
        // execution cannot drift between replicas or later environment reads.
        let execution_policy = super::execution_policy::ProviderExecutionPolicy::capture();
        let client = super::provider_http::secure_provider_client_builder(execution_policy)
            .build()
            .expect("Failed to build HTTP client");
        let rate_limit = execution_policy.rate_limit_ms().map(RateLimiter::new);

        if rate_limit.is_some() {
            tracing::info!(
                "LLM rate limiting enabled: {}ms between calls",
                execution_policy.rate_limit_ms().unwrap_or_default()
            );
        }

        Self {
            client,
            base_url: base_url.unwrap_or_else(|| "https://api.openai.com/v1".into()),
            api_key,
            rate_limit,
            execution_policy,
        }
    }

    fn build_body(&self, request: &ChatRequest, tools: Option<&[ToolSchema]>) -> Value {
        let messages: Vec<Value> = request
            .messages
            .iter()
            .map(|m| {
                let mut msg = json!({"role": m.role});
                // When images are attached, serialize content as an array of
                // content parts (text + image_url blocks). This is the OpenAI
                // vision format, also used by Gemini and other OpenAI-compatible
                // endpoints.
                if let Some(ref images) = m.images {
                    if !images.is_empty() {
                        let mut parts: Vec<serde_json::Value> = Vec::new();
                        if let Some(ref text) = m.content {
                            parts.push(json!({"type": "text", "text": text}));
                        }
                        for img in images {
                            let data_uri = format!("data:{};base64,{}", img.mime_type, img.data);
                            parts.push(json!({
                                "type": "image_url",
                                "image_url": { "url": data_uri }
                            }));
                        }
                        msg["content"] = json!(parts);
                    } else if let Some(ref content) = m.content {
                        msg["content"] = json!(content);
                    }
                } else if let Some(ref content) = m.content {
                    msg["content"] = json!(content);
                }
                if let Some(ref tool_call_id) = m.tool_call_id {
                    msg["tool_call_id"] = json!(tool_call_id);
                }
                if let Some(ref tool_calls) = m.tool_calls {
                    msg["tool_calls"] = serde_json::to_value(tool_calls).unwrap_or_default();
                }
                msg
            })
            .collect();

        let mut body = json!({
            "model": request.model,
            "messages": messages,
        });

        if let Some(temp) = request.temperature {
            body["temperature"] = json!(temp);
        }
        request_body::insert_completion_token_limit(&mut body, &request.model, request.max_tokens);

        if let Some(ref fmt) = request.response_format {
            match fmt {
                ResponseFormat::Text => {
                    body["response_format"] = json!({"type": "text"});
                }
                ResponseFormat::JsonObject => {
                    body["response_format"] = json!({"type": "json_object"});
                }
                ResponseFormat::JsonSchema { name, schema } => {
                    body["response_format"] = json!({
                        "type": "json_schema",
                        "json_schema": {
                            "name": name,
                            "schema": schema,
                            "strict": true,
                        }
                    });
                }
            }
        }

        request_body::insert_tools(&mut body, &request.model, tools);

        if let Some(ref key) = request.prompt_cache_key {
            body["prompt_cache_key"] = json!(key);
        }
        if let Some(ref retention) = request.prompt_cache_retention {
            body["prompt_cache_retention"] = json!(retention);
        }

        body
    }

    fn prepare_request(&self, body: &Value) -> Result<Vec<u8>> {
        if self.api_key.trim().is_empty() {
            return Err(IronCrewError::Validation(
                "OPENAI_API_KEY is required for OpenAI provider".into(),
            ));
        }
        self.execution_policy.serialize_request("OpenAI", body)
    }

    async fn send_request(&self, body: Value) -> Result<ChatResponse> {
        let request_body = self.prepare_request(&body)?;

        // Rate limit: wait if needed
        if let Some(ref limiter) = self.rate_limit {
            limiter.wait().await;
        }

        let url = format!("{}/chat/completions", self.base_url);
        crate::utils::network::validate_url_not_private(&url)
            .map_err(|error| IronCrewError::Provider(format!("Unsafe provider URL: {error}")))?;

        let resp = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .body(request_body)
            .send()
            .await
            .map_err(IronCrewError::Http)?;

        let status = resp.status();
        if !status.is_success() {
            return Err(
                read_error_response(resp, self.execution_policy, "OpenAI error response")
                    .await?
                    .into_error(),
            );
        }

        // Read with a strict byte budget before parsing. This remains resilient
        // to HTTP/2 framing quirks without allowing an unbounded allocation.
        let resp_bytes = crate::utils::http::read_response_bytes(
            resp,
            self.execution_policy.response_bytes(),
            "OpenAI response",
        )
        .await
        .map_err(|error| IronCrewError::Provider(error.to_string()))?;
        let resp_text = String::from_utf8(resp_bytes)
            .map_err(|_| IronCrewError::Provider("OpenAI response was not valid UTF-8".into()))?;
        let resp_body: Value = serde_json::from_str(&resp_text).map_err(|e| {
            tracing::debug!("Failed to parse response as JSON: {}", e);
            tracing::debug!(
                "Raw response body: {}",
                crate::utils::http::utf8_prefix(&resp_text, 500)
            );
            IronCrewError::Provider(format!("Invalid JSON response from LLM provider: {}", e))
        })?;

        let choice = &resp_body["choices"][0]["message"];

        let content = choice["content"].as_str().map(|s| s.to_string());

        // Reasoning content (DeepSeek, Kimi, Moonshot): `reasoning_content`
        // Some OpenAI-compat forks use `reasoning` instead.
        let reasoning = choice["reasoning_content"]
            .as_str()
            .or_else(|| choice["reasoning"].as_str())
            .map(|s| s.to_string());

        // Parse tool calls leniently — providers return different formats:
        // - OpenAI: arguments as JSON string, type="function", id present
        // - Gemini: arguments as object (not string), may omit type/id
        let tool_calls = parse_tool_calls_lenient(choice.get("tool_calls"));

        let usage = resp_body.get("usage").map(|u| TokenUsage {
            prompt_tokens: u["prompt_tokens"].as_u64().unwrap_or(0) as u32,
            completion_tokens: u["completion_tokens"].as_u64().unwrap_or(0) as u32,
            total_tokens: u["total_tokens"].as_u64().unwrap_or(0) as u32,
            cached_tokens: u["prompt_tokens_details"]["cached_tokens"]
                .as_u64()
                .unwrap_or(0) as u32,
        });

        Ok(ChatResponse {
            content,
            reasoning,
            tool_calls,
            usage,
            raw_blocks: None,
        })
    }

    async fn send_request_stream(
        &self,
        mut body: Value,
        tx: tokio::sync::mpsc::Sender<StreamChunk>,
    ) -> Result<ChatResponse> {
        body["stream"] = json!(true);
        let request_body = self.prepare_request(&body)?;

        if let Some(ref limiter) = self.rate_limit {
            limiter.wait().await;
        }

        let url = format!("{}/chat/completions", self.base_url);
        crate::utils::network::validate_url_not_private(&url)
            .map_err(|error| IronCrewError::Provider(format!("Unsafe provider URL: {error}")))?;

        let resp = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .body(request_body)
            .send()
            .await
            .map_err(IronCrewError::Http)?;

        if !resp.status().is_success() {
            return Err(
                read_error_response(resp, self.execution_policy, "OpenAI error response")
                    .await?
                    .into_error(),
            );
        }

        let mut full_content = String::new();
        let mut full_reasoning = String::new();
        let output_limit = self.execution_policy.output_bytes();
        let mut stored_output_bytes = 0_usize;
        // Track tool call assembly (streaming sends deltas)
        let mut tool_call_buffers = stream_tools::StreamToolCalls::default();

        let mut lines = ProviderSseLines::new(resp, self.execution_policy, "OpenAI stream");
        // Track terminal delivery: a mid-stream `{"error": …}` chunk or a stream
        // that ends before `data: [DONE]` must fail rather than return whatever
        // partial content accumulated.
        let mut stream_error: Option<String> = None;
        let mut saw_done = false;

        while let Some(raw_line) = lines.next_line().await? {
            let line = raw_line.trim();

            if line.is_empty() {
                continue;
            }

            if line == "data: [DONE]" {
                saw_done = true;
                let _ = tx.send(StreamChunk::Done).await;
                continue;
            }

            if let Some(data) = sse_field(line, "data")
                && let Ok(parsed) = serde_json::from_str::<Value>(data)
            {
                // Some OpenAI-compatible servers report failures as an inline
                // `{"error": {...}}` data event mid-stream.
                if let Some(err) = parsed.get("error").filter(|e| !e.is_null()) {
                    let msg = err["message"]
                        .as_str()
                        .or_else(|| err.as_str())
                        .unwrap_or("stream error");
                    stream_error = Some(msg.to_string());
                    break;
                }

                let delta = &parsed["choices"][0]["delta"];

                // Text content delta
                if let Some(content) = delta["content"].as_str() {
                    crate::utils::http::bounded_push_str(
                        &mut full_content,
                        content,
                        &mut stored_output_bytes,
                        output_limit,
                        "OpenAI accumulated output",
                    )
                    .map_err(|error| IronCrewError::Provider(error.to_string()))?;
                    let _ = tx.send(StreamChunk::Text(content.to_string())).await;
                }

                // Reasoning delta (DeepSeek, Kimi, Moonshot use reasoning_content)
                if let Some(reasoning) = delta["reasoning_content"]
                    .as_str()
                    .or_else(|| delta["reasoning"].as_str())
                {
                    crate::utils::http::bounded_push_str(
                        &mut full_reasoning,
                        reasoning,
                        &mut stored_output_bytes,
                        output_limit,
                        "OpenAI accumulated output",
                    )
                    .map_err(|error| IronCrewError::Provider(error.to_string()))?;
                    let _ = tx.send(StreamChunk::Thinking(reasoning.to_string())).await;
                }

                // Tool calls delta
                if let Some(tc_deltas) = delta.get("tool_calls").and_then(|v| v.as_array()) {
                    for tc in tc_deltas {
                        let updates = tool_call_buffers.apply_delta(
                            tc,
                            &mut stored_output_bytes,
                            output_limit,
                        )?;
                        if let Some((id, name)) = updates.start {
                            let _ = tx.try_send(StreamChunk::ToolCallStart { id, name });
                        }
                        if let Some((id, arguments_delta)) = updates.arguments {
                            let _ = tx.try_send(StreamChunk::ToolCallDelta {
                                id,
                                arguments_delta,
                            });
                        }
                    }
                }
            }
        }

        if let Some(err) = stream_error {
            return Err(IronCrewError::Provider(format!(
                "OpenAI stream error — {err}"
            )));
        }

        // Assemble tool calls from buffers
        let tool_calls = tool_call_buffers.finish()?;

        // A stream that ended before `[DONE]` *and* produced nothing is a
        // truncated/dropped connection — fail with a clear message instead of
        // the misleading "Empty response from LLM" downstream. We don't fail a
        // content-bearing stream on a missing `[DONE]`, since some
        // OpenAI-compatible providers omit that terminal marker.
        if !saw_done && full_content.is_empty() && tool_calls.is_empty() {
            return Err(IronCrewError::Provider(
                "OpenAI stream ended before [DONE] with no content (truncated response)".into(),
            ));
        }

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

        Ok(ChatResponse {
            content,
            reasoning,
            tool_calls,
            usage: None,
            raw_blocks: None,
        })
    }
}

/// Parse tool calls leniently to handle different provider response formats.
/// - OpenAI: `arguments` is a JSON string, `type` is "function", `id` is present
/// - Gemini: `arguments` may be a JSON object (not string), `type`/`id` may be missing
fn parse_tool_calls_lenient(tool_calls_value: Option<&Value>) -> Vec<ToolCallRequest> {
    let Some(tc_array) = tool_calls_value.and_then(|v| v.as_array()) else {
        return Vec::new();
    };

    tc_array
        .iter()
        .filter_map(|tc| {
            let id = tc["id"].as_str().unwrap_or("").to_string();
            let call_type = tc["type"].as_str().unwrap_or("function").to_string();

            let name = tc["function"]["name"].as_str()?.to_string();

            // Handle arguments as either a string (OpenAI) or an object (Gemini)
            let arguments = match &tc["function"]["arguments"] {
                Value::String(s) => s.clone(),
                Value::Object(_) | Value::Array(_) => {
                    serde_json::to_string(&tc["function"]["arguments"]).unwrap_or_default()
                }
                _ => String::from("{}"),
            };

            Some(ToolCallRequest {
                id,
                call_type,
                function: ToolCallFunction { name, arguments },
            })
        })
        .collect()
}

#[async_trait]
impl LlmProvider for OpenAiProvider {
    fn metrics_family(&self) -> crate::metrics::ProviderFamily {
        crate::metrics::ProviderFamily::OpenAi
    }

    fn execution_fingerprint(&self) -> Result<String> {
        crate::engine::conversation_provider::provider_execution_fingerprint(
            "openai",
            &self.base_url,
            &serde_json::json!({
                "execution_policy": self.execution_policy.definition(),
            }),
        )
    }

    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse> {
        tracing::debug!(
            provider = "openai",
            model = %request.model,
            messages = request.messages.len(),
            estimated_message_bytes = chat_history_estimated_bytes(&request.messages),
            tools = 0,
            "LLM request metadata"
        );
        let body = self.build_body(&request, None);
        let response = self.send_request(body).await?;
        tracing::debug!(
            provider = "openai",
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
            provider = "openai",
            model = %request.model,
            messages = request.messages.len(),
            estimated_message_bytes = chat_history_estimated_bytes(&request.messages),
            tools = tools.len(),
            "LLM request metadata"
        );
        let body = self.build_body(&request, Some(tools));
        let response = self.send_request(body).await?;
        tracing::debug!(
            provider = "openai",
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
        let body = self.build_body(&request, None);
        tracing::debug!("LLM streaming request");
        self.send_request_stream(body, tx).await
    }
}
