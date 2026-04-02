use serde::Serialize;
use std::sync::Arc;
use tokio::sync::{RwLock, broadcast};

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "event", content = "data")]
#[allow(dead_code)]
pub enum CrewEvent {
    // ─── Crew lifecycle ─────────────────────────────────────────────────────
    #[serde(rename = "crew_started")]
    CrewStarted {
        goal: String,
        agent_count: usize,
        task_count: usize,
        model: String,
    },

    // ─── Phase lifecycle ────────────────────────────────────────────────────
    #[serde(rename = "phase_start")]
    PhaseStart { phase: usize, tasks: Vec<String> },

    // ─── Task lifecycle ─────────────────────────────────────────────────────
    #[serde(rename = "task_assigned")]
    TaskAssigned {
        task: String,
        agent: String,
        phase: usize,
    },

    #[serde(rename = "task_completed")]
    TaskCompleted {
        task: String,
        agent: String,
        duration_ms: u64,
        success: bool,
        output: String,
        token_usage: Option<TokenUsageSummary>,
    },

    #[serde(rename = "task_failed")]
    TaskFailed {
        task: String,
        agent: String,
        error: String,
        duration_ms: u64,
    },

    #[serde(rename = "task_skipped")]
    TaskSkipped { task: String, reason: String },

    #[serde(rename = "task_retry")]
    TaskRetry {
        task: String,
        attempt: u32,
        max_retries: u32,
        backoff_secs: f64,
        error: String,
    },

    // ─── Tool calls ─────────────────────────────────────────────────────────
    #[serde(rename = "tool_call")]
    ToolCall { task: String, tool: String },

    #[serde(rename = "tool_result")]
    ToolResult {
        task: String,
        tool: String,
        success: bool,
        duration_ms: u64,
    },

    // ─── Agent messages ─────────────────────────────────────────────────────
    #[serde(rename = "message_sent")]
    MessageSent {
        from: String,
        to: String,
        message_type: String,
    },

    // ─── Collaborative ──────────────────────────────────────────────────────
    #[serde(rename = "collaboration_turn")]
    CollaborationTurn {
        task: String,
        agent: String,
        turn: usize,
        content: String,
    },

    // ─── Memory ─────────────────────────────────────────────────────────────
    #[serde(rename = "memory_set")]
    MemorySet { key: String },

    // ─── Logging ────────────────────────────────────────────────────────────
    #[serde(rename = "log")]
    Log { level: String, message: String },

    // ─── Run complete ───────────────────────────────────────────────────────
    #[serde(rename = "run_complete")]
    RunComplete {
        run_id: String,
        status: String,
        duration_ms: u64,
        total_tokens: u32,
    },
}

#[derive(Debug, Clone, Serialize)]
pub struct TokenUsageSummary {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
    pub cached_tokens: u32,
}

#[derive(Clone)]
pub struct EventBus {
    sender: Arc<broadcast::Sender<CrewEvent>>,
    /// Replay buffer: emitted events stored for late subscribers (capped).
    history: Arc<RwLock<Vec<CrewEvent>>>,
    /// Maximum number of events to keep in the replay buffer.
    max_replay: usize,
}

impl EventBus {
    pub fn new(capacity: usize) -> Self {
        let (sender, _) = broadcast::channel(capacity);
        Self {
            sender: Arc::new(sender),
            history: Arc::new(RwLock::new(Vec::new())),
            max_replay: 1000,
        }
    }

    pub fn emit(&self, event: CrewEvent) {
        // Store in replay buffer
        if let Ok(mut history) = self.history.try_write() {
            history.push(event.clone());
            // Trim oldest if over cap
            if history.len() > self.max_replay {
                let excess = history.len() - self.max_replay;
                history.drain(..excess);
            }
        }
        // Broadcast to live subscribers (ignore if none)
        let _ = self.sender.send(event);
    }

    pub fn subscribe(&self) -> broadcast::Receiver<CrewEvent> {
        self.sender.subscribe()
    }

    /// Get all events emitted so far (for replay to late subscribers).
    pub async fn replay(&self) -> Vec<CrewEvent> {
        self.history.read().await.clone()
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new(256)
    }
}
