//! OpenAI Responses API provider.
//!
//! Implements the `/v1/responses` endpoint (OpenAI, Azure OpenAI, xAI/Grok,
//! OpenRouter). This endpoint is stateful (via `previous_response_id`) and
//! exposes reasoning items, built-in server-side tools (web_search,
//! file_search, code_interpreter, MCP), and cleaner streaming semantics.
//!
//! Tasks are stateless: full message history is sent without `previous_response_id`.

use async_trait::async_trait;
use reqwest::Client;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};

use super::accounting::ProviderAttempt;
use super::provider::*;
use crate::usage::{ProviderUsage, UsageTracker};
mod stream;
use super::provider_http::{ProviderSseLines, RateLimiter, read_error_response, sse_field};
mod request_body;
use crate::utils::error::{IronCrewError, Result};

/// OpenAI Responses API-specific configuration.
#[derive(Debug, Clone, Default)]
pub struct ResponsesConfig {
    /// Crew-level effort; validated against the effective model's capabilities.
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

pub struct OpenAiResponsesProvider {
    client: Client,
    base_url: String,
    api_key: String,
    rate_limit: Option<RateLimiter>,
    config: ResponsesConfig,
    execution_policy: super::execution_policy::ProviderExecutionPolicy,
}

impl OpenAiResponsesProvider {
    pub fn new(api_key: String, base_url: Option<String>, config: ResponsesConfig) -> Self {
        let execution_policy = super::execution_policy::ProviderExecutionPolicy::capture();
        let client = super::provider_http::secure_provider_client_builder(execution_policy)
            .build()
            .expect("Failed to build HTTP client");
        let rate_limit = execution_policy.rate_limit_ms().map(RateLimiter::new);

        Self {
            client,
            base_url: base_url.unwrap_or_else(|| "https://api.openai.com".into()),
            api_key,
            rate_limit,
            config,
            execution_policy,
        }
    }

    fn prepare_request(&self, body: &Value) -> Result<Vec<u8>> {
        if self.api_key.trim().is_empty() {
            return Err(IronCrewError::Validation(
                "API key is required for OpenAI Responses provider".into(),
            ));
        }
        self.execution_policy
            .serialize_request("OpenAI Responses", body)
    }

    /// Detect if the base_url points to xAI/Grok (which doesn't support `instructions` param).
    fn is_grok(&self) -> bool {
        self.base_url.contains("x.ai")
    }

    /// Send a capability-checked Responses request within captured HTTP limits.
    async fn send_request(
        &self,
        body: Value,
        usage_tracker: Option<&UsageTracker>,
    ) -> Result<ChatResponse> {
        let request_body = self.prepare_request(&body)?;

        if let Some(ref limiter) = self.rate_limit {
            limiter.wait().await;
        }

        let url = format!("{}/v1/responses", self.base_url);
        crate::utils::network::validate_url_not_private(&url)
            .map_err(|error| IronCrewError::Provider(format!("Unsafe provider URL: {error}")))?;

        let mut accounting = ProviderAttempt::start(usage_tracker, ProviderUsage::OpenAiResponses)?;
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
            return Err(read_error_response(
                resp,
                self.execution_policy,
                "OpenAI Responses error response",
            )
            .await?
            .into_accounted_error(&mut accounting));
        }
        let bytes = crate::utils::http::read_response_bytes(
            resp,
            self.execution_policy.response_bytes(),
            "OpenAI Responses response",
        )
        .await
        .map_err(|error| IronCrewError::Provider(error.to_string()))?;
        let resp_text = String::from_utf8(bytes).map_err(|_| {
            IronCrewError::Provider("OpenAI Responses response was not valid UTF-8".into())
        })?;
        let resp_body: Value = serde_json::from_str(&resp_text).map_err(|e| {
            tracing::debug!(
                "Raw response: {}",
                crate::utils::http::utf8_prefix(&resp_text, 500)
            );
            IronCrewError::Provider(format!("Invalid JSON from Responses API: {}", e))
        })?;

        let terminal_usage = matches!(
            resp_body["status"].as_str(),
            Some("completed" | "failed" | "incomplete" | "cancelled")
        );
        accounting.observe(resp_body.get("usage"), terminal_usage);
        let response = parse_responses_response(&resp_body)?;
        accounting.finish()?;
        Ok(response)
    }
}

mod response;
use response::parse_responses_response;

#[async_trait]
impl LlmProvider for OpenAiResponsesProvider {
    fn records_usage(&self) -> bool {
        true
    }

    fn validate_request(&self, request: &ChatRequest, has_tools: bool) -> Result<()> {
        self.resolve_options(request, has_tools).map(|_| ())
    }

    fn metrics_family(&self) -> crate::metrics::ProviderFamily {
        crate::metrics::ProviderFamily::OpenAiResponses
    }

    fn execution_fingerprint(&self) -> Result<String> {
        let server_tools = self
            .config
            .server_tools
            .iter()
            .map(|tool| match tool {
                ServerTool::WebSearch { context_size } => {
                    json!({"type": "web_search", "context_size": context_size})
                }
                ServerTool::FileSearch {
                    vector_store_ids,
                    max_num_results,
                } => json!({
                    "type": "file_search",
                    "vector_store_ids": vector_store_ids,
                    "max_num_results": max_num_results,
                }),
                ServerTool::CodeInterpreter => json!({"type": "code_interpreter"}),
                ServerTool::Mcp {
                    server_label,
                    server_url,
                    allowed_tools,
                } => json!({
                    "type": "mcp",
                    "server_label": server_label,
                    "server_url": server_url,
                    "allowed_tools": allowed_tools,
                }),
            })
            .collect::<Vec<_>>();
        crate::engine::conversation_provider::provider_execution_fingerprint(
            "openai-responses",
            &self.base_url,
            &json!({
                "reasoning_effort": self.config.reasoning_effort,
                "reasoning_summary": self.config.reasoning_summary,
                "server_tools": server_tools,
                "capability_policy": super::capabilities::REVISION,
                "execution_policy": self.execution_policy.definition(),
            }),
        )
    }

    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse> {
        tracing::debug!(
            provider = "openai-responses",
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
            provider = "openai-responses",
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
            provider = "openai-responses",
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
            provider = "openai-responses",
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
        let body = self.build_body(&request, None)?;
        tracing::debug!("Responses API streaming request");
        self.send_request_stream(body, request.usage_tracker.as_ref(), tx)
            .await
    }
}

#[cfg(test)]
mod tests;
