use async_trait::async_trait;
use reqwest::Client;
use serde_json::{Value, json};
use std::collections::BTreeMap;

use super::accounting::ProviderAttempt;
use super::provider::*;
use crate::usage::{ProviderUsage, UsageTracker};
mod stream;
use super::provider_http::{ProviderSseLines, RateLimiter, read_error_response, sse_field};
mod request_body;
use crate::utils::error::{IronCrewError, Result};

mod response;
use response::{parse_anthropic_response, structured_output_tool_name};

mod config;
pub use config::{AnthropicConfig, ServerTool};

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
        let execution_policy = super::execution_policy::ProviderExecutionPolicy::capture();
        // Every request carries the API key in `x-api-key`, which reqwest does
        // not strip across hosts, so a redirect must never leave the origin.
        let client = super::provider_http::secure_provider_client_builder(execution_policy)
            .redirect(crate::utils::network::same_origin_redirect_policy(
                crate::utils::network::OutboundNetworkPolicy::PublicOnly,
                crate::utils::network::private_ips_override_enabled(),
            ))
            .build()
            .expect("Failed to build HTTP client");

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

    /// Options in the body have already passed the shared capability policy.
    /// Send a non-streaming request to the Anthropic Messages API.
    async fn send_request(
        &self,
        body: Value,
        structured_output_tool: Option<&str>,
        usage_tracker: Option<&UsageTracker>,
    ) -> Result<ChatResponse> {
        let request_body = self.prepare_request(&body)?;

        if let Some(ref limiter) = self.rate_limit {
            limiter.wait().await;
        }

        let url = format!("{}/v1/messages", self.base_url);
        crate::utils::network::validate_url_not_private(&url)
            .map_err(|error| IronCrewError::Provider(format!("Unsafe provider URL: {error}")))?;

        let mut accounting = ProviderAttempt::start(usage_tracker, ProviderUsage::Anthropic)?;
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
        if !status.is_success() {
            return Err(read_error_response(
                resp,
                self.execution_policy,
                "Anthropic error response",
            )
            .await?
            .into_accounted_error(&mut accounting));
        }
        let bytes = crate::utils::http::read_response_bytes(
            resp,
            self.execution_policy.response_bytes(),
            "Anthropic response",
        )
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

        accounting.observe(resp_body.get("usage"), true);
        let response = parse_anthropic_response(&resp_body, structured_output_tool)?;
        accounting.finish()?;
        Ok(response)
    }
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
    fn records_usage_metrics(&self) -> bool {
        true
    }

    fn records_usage(&self) -> bool {
        true
    }

    fn validate_request(&self, request: &ChatRequest, has_tools: bool) -> Result<()> {
        super::capabilities::anthropic(
            &self.base_url,
            request,
            self.config.thinking_budget,
            has_tools || !self.config.server_tools.is_empty(),
        )
    }

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
                "capability_policy": super::capabilities::REVISION,
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
        let body = self.build_body(&request, None)?;
        let response = self
            .send_request(body, structured_output_tool, request.usage_tracker.as_ref())
            .await?;
        tracing::debug!(
            provider = "anthropic",
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
            provider = "anthropic",
            model = %request.model,
            messages = request.messages.len(),
            estimated_message_bytes = chat_history_estimated_bytes(&request.messages),
            tools = tools.len(),
            "LLM request metadata"
        );
        let structured_output_tool = structured_output_tool_name(&request);
        let body = self.build_body(&request, Some(tools))?;
        let response = self
            .send_request(body, structured_output_tool, request.usage_tracker.as_ref())
            .await?;
        tracing::debug!(
            provider = "anthropic",
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
        let structured_output_tool = structured_output_tool_name(&request);
        let body = self.build_body(&request, None)?;
        tracing::debug!("Anthropic streaming request");
        self.send_request_stream(
            body,
            structured_output_tool,
            request.usage_tracker.as_ref(),
            tx,
        )
        .await
    }
}

#[cfg(test)]
mod tests;
