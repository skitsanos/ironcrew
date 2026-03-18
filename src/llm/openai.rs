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

    async fn send_request_stream(
        &self,
        mut body: Value,
        tx: tokio::sync::mpsc::Sender<StreamChunk>,
    ) -> Result<ChatResponse> {
        body["stream"] = json!(true);

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

        if !resp.status().is_success() {
            let status = resp.status();
            let error_body: Value = resp.json().await.map_err(IronCrewError::Http)?;
            let error_msg = error_body["error"]["message"]
                .as_str()
                .unwrap_or("Unknown API error");
            return Err(IronCrewError::Provider(format!(
                "HTTP {}: {}",
                status, error_msg
            )));
        }

        let mut full_content = String::new();
        // Track tool call assembly (streaming sends deltas)
        let mut tool_call_buffers: std::collections::HashMap<usize, (String, String, String)> =
            std::collections::HashMap::new(); // index -> (id, name, arguments)

        // Read SSE stream
        let mut stream = resp.bytes_stream();
        use futures::StreamExt;
        let mut buffer = String::new();

        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(IronCrewError::Http)?;
            let text = String::from_utf8_lossy(&chunk);
            buffer.push_str(&text);

            // Process complete lines
            while let Some(newline_pos) = buffer.find('\n') {
                let line = buffer[..newline_pos].trim().to_string();
                buffer = buffer[newline_pos + 1..].to_string();

                if line.is_empty() {
                    continue;
                }

                if line == "data: [DONE]" {
                    let _ = tx.send(StreamChunk::Done).await;
                    continue;
                }

                if let Some(data) = line.strip_prefix("data: ")
                    && let Ok(parsed) = serde_json::from_str::<Value>(data)
                {
                    let delta = &parsed["choices"][0]["delta"];

                    // Text content delta
                    if let Some(content) = delta["content"].as_str() {
                        full_content.push_str(content);
                        let _ = tx.send(StreamChunk::Text(content.to_string())).await;
                    }

                    // Tool calls delta
                    if let Some(tc_deltas) =
                        delta.get("tool_calls").and_then(|v| v.as_array())
                    {
                        for tc in tc_deltas {
                            let index = tc["index"].as_u64().unwrap_or(0) as usize;
                            let entry = tool_call_buffers
                                .entry(index)
                                .or_insert_with(|| (String::new(), String::new(), String::new()));

                            if let Some(id) = tc["id"].as_str() {
                                entry.0 = id.to_string();
                                if let Some(name) = tc["function"]["name"].as_str() {
                                    entry.1 = name.to_string();
                                    let _ = tx.try_send(StreamChunk::ToolCallStart {
                                        id: id.to_string(),
                                        name: name.to_string(),
                                    });
                                }
                            }

                            if let Some(args_delta) = tc["function"]["arguments"].as_str() {
                                entry.2.push_str(args_delta);
                                let _ = tx.try_send(StreamChunk::ToolCallDelta {
                                    id: entry.0.clone(),
                                    arguments_delta: args_delta.to_string(),
                                });
                            }
                        }
                    }
                }
            }
        }

        // Assemble tool calls from buffers
        let tool_calls: Vec<ToolCallRequest> = tool_call_buffers
            .into_values()
            .filter(|(id, name, _)| !id.is_empty() && !name.is_empty())
            .map(|(id, name, arguments)| ToolCallRequest {
                id,
                call_type: "function".to_string(),
                function: ToolCallFunction { name, arguments },
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
