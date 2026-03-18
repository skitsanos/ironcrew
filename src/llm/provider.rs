use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::engine::agent::ResponseFormat;
use crate::utils::error::Result;

#[derive(Debug, Clone, Serialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: Option<String>,
    /// For role="tool" messages: the tool_call_id this result corresponds to
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// For role="assistant" messages: tool calls requested by the model
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCallRequest>>,
}

impl ChatMessage {
    pub fn system(content: &str) -> Self {
        Self { role: "system".into(), content: Some(content.into()), tool_call_id: None, tool_calls: None }
    }
    pub fn user(content: &str) -> Self {
        Self { role: "user".into(), content: Some(content.into()), tool_call_id: None, tool_calls: None }
    }
    pub fn assistant(content: Option<String>, tool_calls: Option<Vec<ToolCallRequest>>) -> Self {
        Self { role: "assistant".into(), content, tool_call_id: None, tool_calls }
    }
    pub fn tool(tool_call_id: &str, content: &str) -> Self {
        Self { role: "tool".into(), content: Some(content.into()), tool_call_id: Some(tool_call_id.into()), tool_calls: None }
    }
}

#[derive(Debug, Clone)]
pub struct ChatRequest {
    pub messages: Vec<ChatMessage>,
    pub model: String,
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
    pub response_format: Option<ResponseFormat>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallRequest {
    pub id: String,
    #[serde(rename = "type")]
    pub call_type: String,
    pub function: ToolCallFunction,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallFunction {
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Clone)]
pub struct ChatResponse {
    pub content: Option<String>,
    pub tool_calls: Vec<ToolCallRequest>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolSchema {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

/// A chunk of a streaming response.
#[derive(Debug, Clone)]
#[allow(dead_code)] // variants and fields are used via channel send/receive across modules
pub enum StreamChunk {
    /// A text delta (partial content)
    Text(String),
    /// A tool call starting
    ToolCallStart { id: String, name: String },
    /// Tool call arguments delta
    ToolCallDelta { id: String, arguments_delta: String },
    /// Stream finished
    Done,
    /// Error during streaming
    Error(String),
}

#[async_trait]
pub trait LlmProvider: Send + Sync {
    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse>;
    async fn chat_with_tools(
        &self,
        request: ChatRequest,
        tools: &[ToolSchema],
    ) -> Result<ChatResponse>;

    /// Stream a chat response. Default implementation falls back to non-streaming.
    async fn chat_stream(
        &self,
        request: ChatRequest,
        tx: tokio::sync::mpsc::Sender<StreamChunk>,
    ) -> Result<ChatResponse> {
        let response = self.chat(request).await?;
        if let Some(ref content) = response.content {
            let _ = tx.send(StreamChunk::Text(content.clone())).await;
        }
        let _ = tx.send(StreamChunk::Done).await;
        Ok(response)
    }
}
