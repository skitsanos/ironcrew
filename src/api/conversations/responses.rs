use serde::{Deserialize, Serialize};

#[derive(Clone, Deserialize, Serialize)]
pub struct MessageResp {
    pub request_usage: crate::usage::UsageSnapshot,
    pub usage: crate::usage::UsageSnapshot,
    pub conversation_id: String,
    pub turn_index: usize,
    pub assistant: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
    pub turn_count: usize,
    pub revision: u64,
    pub incarnation_id: String,
    pub definition_fingerprint: String,
}

#[derive(Serialize)]
pub struct HistoryResp {
    pub usage: crate::usage::UsageSnapshot,
    pub conversation_id: String,
    pub flow: Option<String>,
    pub agent: String,
    pub created_at: String,
    pub updated_at: String,
    pub messages: Vec<HistoryMessage>,
    pub turn_count: usize,
    pub truncated: bool,
    pub revision: u64,
    pub incarnation_id: String,
    pub source_fingerprint: String,
    pub definition_fingerprint: String,
}

#[derive(Serialize)]
pub struct HistoryMessage {
    pub role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}
