use async_trait::async_trait;
use reqwest::Client;
use serde_json::{Value, json};

use super::accounting::ProviderAttempt;
use super::provider::*;
use crate::usage::{ProviderUsage, UsageTracker};
mod stream;
use super::provider_http::{ProviderSseLines, RateLimiter, read_error_response, sse_field};
use crate::utils::error::{IronCrewError, Result};

#[cfg(test)]
mod capability_tests;
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

    fn prepare_request(&self, body: &Value) -> Result<Vec<u8>> {
        if self.api_key.trim().is_empty() {
            return Err(IronCrewError::Validation(
                "OPENAI_API_KEY is required for OpenAI provider".into(),
            ));
        }
        self.execution_policy.serialize_request("OpenAI", body)
    }

    async fn send_request(
        &self,
        body: Value,
        usage_tracker: Option<&UsageTracker>,
    ) -> Result<ChatResponse> {
        let request_body = self.prepare_request(&body)?;

        // Rate limit: wait if needed
        if let Some(ref limiter) = self.rate_limit {
            limiter.wait().await;
        }

        let url = format!("{}/chat/completions", self.base_url);
        crate::utils::network::validate_url_not_private(&url)
            .map_err(|error| IronCrewError::Provider(format!("Unsafe provider URL: {error}")))?;

        let mut accounting = ProviderAttempt::start(usage_tracker, ProviderUsage::OpenAiChat)?;
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
                    .into_accounted_error(&mut accounting),
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

        accounting.observe(resp_body.get("usage"), true);
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

        let usage = accounting.finish()?;
        Ok(ChatResponse {
            content,
            reasoning,
            tool_calls,
            usage,
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
    fn records_usage_metrics(&self) -> bool {
        true
    }

    fn records_usage(&self) -> bool {
        true
    }

    fn validate_request(&self, request: &ChatRequest, has_tools: bool) -> Result<()> {
        self.resolve_options(request, has_tools).map(|_| ())
    }

    fn metrics_family(&self) -> crate::metrics::ProviderFamily {
        crate::metrics::ProviderFamily::OpenAi
    }

    fn execution_fingerprint(&self) -> Result<String> {
        crate::engine::conversation_provider::provider_execution_fingerprint(
            "openai",
            &self.base_url,
            &serde_json::json!({
                "capability_policy": super::capabilities::REVISION,
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
        let body = self.build_body(&request, None)?;
        let response = self
            .send_request(body, request.usage_tracker.as_ref())
            .await?;
        tracing::debug!(
            provider = "openai",
            content_bytes = response.content.as_ref().map_or(0, String::len),
            reasoning_bytes = response.reasoning.as_ref().map_or(0, String::len),
            tool_calls = response.tool_calls.len(),
            raw_blocks = response.raw_blocks.as_ref().map_or(0, Vec::len),
            total_tokens = ?response.usage.counts().total_tokens,
            usage_coverage = ?response.usage.coverage(),
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
        let body = self.build_body(&request, Some(tools))?;
        let response = self
            .send_request(body, request.usage_tracker.as_ref())
            .await?;
        tracing::debug!(
            provider = "openai",
            content_bytes = response.content.as_ref().map_or(0, String::len),
            reasoning_bytes = response.reasoning.as_ref().map_or(0, String::len),
            tool_calls = response.tool_calls.len(),
            raw_blocks = response.raw_blocks.as_ref().map_or(0, Vec::len),
            total_tokens = ?response.usage.counts().total_tokens,
            usage_coverage = ?response.usage.coverage(),
            "LLM response metadata"
        );
        Ok(response)
    }

    async fn chat_stream(
        &self,
        request: ChatRequest,
        tx: tokio::sync::mpsc::Sender<StreamChunk>,
    ) -> Result<ChatResponse> {
        let body = self.build_body(&request, None)?;
        tracing::debug!("LLM streaming request");
        self.send_request_stream(body, request.usage_tracker.as_ref(), tx)
            .await
    }
}
