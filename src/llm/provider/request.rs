use super::{ChatMessage, ResponseFormat};

#[derive(Debug, Clone)]
pub struct ChatRequest {
    /// Optional process-local scope. Built-in providers settle one dispatched
    /// HTTP attempt here, including failures and cancellation. Callers must
    /// explicitly share it across retries or nested work; nothing is global.
    pub usage_tracker: Option<crate::usage::UsageTracker>,
    pub messages: Vec<ChatMessage>,
    pub model: String,
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
    pub response_format: Option<ResponseFormat>,
    pub prompt_cache_key: Option<String>,
    pub prompt_cache_retention: Option<String>,
    /// Per-request reasoning effort (agent-level override; see `Agent`).
    pub reasoning_effort: Option<String>,
}
