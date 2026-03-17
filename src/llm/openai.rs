use async_trait::async_trait;
use reqwest::Client;
use serde_json::{json, Value};
use std::time::Duration;

use super::provider::*;
use crate::engine::agent::ResponseFormat;
use crate::utils::error::{IronCrewError, Result};

pub struct OpenAiProvider {
    client: Client,
    base_url: String,
    api_key: String,
}

impl OpenAiProvider {
    pub fn new(api_key: String, base_url: Option<String>) -> Self {
        let client = Client::builder()
            .timeout(Duration::from_secs(120))
            .build()
            .expect("Failed to build HTTP client");

        Self {
            client,
            base_url: base_url.unwrap_or_else(|| "https://api.openai.com/v1".into()),
            api_key,
        }
    }

    fn build_body(&self, request: &ChatRequest, tools: Option<&[ToolSchema]>) -> Value {
        let messages: Vec<Value> = request
            .messages
            .iter()
            .map(|m| {
                let mut msg = json!({"role": m.role});
                if let Some(ref content) = m.content {
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
        if let Some(max) = request.max_tokens {
            body["max_tokens"] = json!(max);
        }

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

        if let Some(tool_schemas) = tools {
            let tools_json: Vec<Value> = tool_schemas
                .iter()
                .map(|t| {
                    json!({
                        "type": "function",
                        "function": {
                            "name": t.name,
                            "description": t.description,
                            "parameters": t.parameters,
                        }
                    })
                })
                .collect();
            body["tools"] = json!(tools_json);
        }

        body
    }

    async fn send_request(&self, body: Value) -> Result<ChatResponse> {
        let url = format!("{}/chat/completions", self.base_url);

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
        let resp_body: Value = resp.json().await.map_err(IronCrewError::Http)?;

        if !status.is_success() {
            let error_msg = resp_body["error"]["message"]
                .as_str()
                .unwrap_or("Unknown API error");
            return Err(IronCrewError::Provider(format!(
                "HTTP {}: {}",
                status, error_msg
            )));
        }

        let choice = &resp_body["choices"][0]["message"];

        let content = choice["content"].as_str().map(|s| s.to_string());

        let tool_calls: Vec<ToolCallRequest> = choice
            .get("tool_calls")
            .and_then(|tc| serde_json::from_value(tc.clone()).ok())
            .unwrap_or_default();

        Ok(ChatResponse {
            content,
            tool_calls,
        })
    }
}

#[async_trait]
impl LlmProvider for OpenAiProvider {
    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse> {
        let body = self.build_body(&request, None);
        tracing::debug!("LLM request: {}", serde_json::to_string_pretty(&body).unwrap_or_default());
        let response = self.send_request(body).await?;
        tracing::debug!("LLM response: {:?}", response);
        Ok(response)
    }

    async fn chat_with_tools(
        &self,
        request: ChatRequest,
        tools: &[ToolSchema],
    ) -> Result<ChatResponse> {
        let body = self.build_body(&request, Some(tools));
        tracing::debug!("LLM request (with tools): {}", serde_json::to_string_pretty(&body).unwrap_or_default());
        let response = self.send_request(body).await?;
        tracing::debug!("LLM response: {:?}", response);
        Ok(response)
    }
}
