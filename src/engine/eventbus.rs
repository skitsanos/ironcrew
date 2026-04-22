use serde::Serialize;
use std::collections::VecDeque;
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

    #[serde(rename = "task_thinking")]
    TaskThinking {
        task: String,
        agent: String,
        content: String,
    },

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

    // ─── Agent-as-tool lifecycle ────────────────────────────────────────────
    /// Bracket event fired when an orchestrator agent invokes another agent
    /// via `agent__<name>` as a tool. `caller` is the orchestrator's name;
    /// `callee` is the invoked agent's name (bare, without the `agent__`
    /// prefix). Emitted once, immediately before the sub-agent runs.
    #[serde(rename = "agent_tool_started")]
    AgentToolStarted {
        caller: String,
        callee: String,
        prompt: String,
    },

    /// Bracket event fired when an agent-as-tool invocation completes.
    /// `success` is false only if the invocation errored out at the
    /// Rust/provider level — a sub-agent that returned a low-quality
    /// reply still counts as success.
    #[serde(rename = "agent_tool_completed")]
    AgentToolCompleted {
        caller: String,
        callee: String,
        duration_ms: u64,
        success: bool,
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

    // ─── Conversation (single-agent multi-turn chat) ────────────────────────
    #[serde(rename = "conversation_started")]
    ConversationStarted {
        conversation_id: String,
        agent: String,
    },

    #[serde(rename = "conversation_turn")]
    ConversationTurn {
        conversation_id: String,
        agent: String,
        turn_index: usize,
        user_message: String,
        assistant_message: String,
    },

    #[serde(rename = "conversation_thinking")]
    ConversationThinking {
        conversation_id: String,
        agent: String,
        turn_index: usize,
        content: String,
    },

    // ─── Dialog (agent-to-agent) ────────────────────────────────────────────
    #[serde(rename = "dialog_started")]
    DialogStarted {
        dialog_id: String,
        /// All participating agents in turn order.
        agents: Vec<String>,
        max_turns: usize,
    },

    #[serde(rename = "dialog_turn")]
    DialogTurn {
        dialog_id: String,
        turn_index: usize,
        speaker: String,
        agent: String,
        content: String,
    },

    #[serde(rename = "dialog_thinking")]
    DialogThinking {
        dialog_id: String,
        turn_index: usize,
        speaker: String,
        agent: String,
        content: String,
    },

    #[serde(rename = "dialog_completed")]
    DialogCompleted {
        dialog_id: String,
        total_turns: usize,
        /// Why the dialog ended. `None` means it ran to `max_turns` normally.
        /// When the dialog was stopped early by a `should_stop` callback, this
        /// carries the reason string that the callback returned (or a generic
        /// marker if the callback returned `true` without a reason).
        #[serde(skip_serializing_if = "Option::is_none")]
        stop_reason: Option<String>,
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

/// One entry in the replay buffer — an event plus its approximate serialized size.
/// Size is tracked to enforce the byte budget in `IRONCREW_EVENT_REPLAY_MAX_BYTES`.
type ReplayEntry = (Arc<CrewEvent>, usize);

#[derive(Clone)]
pub struct EventBus {
    sender: Arc<broadcast::Sender<Arc<CrewEvent>>>,
    /// Replay buffer: emitted events stored for late subscribers (capped).
    /// Each entry pairs the event with its approximate serialized size so the
    /// byte budget can be enforced without re-serializing on eviction.
    history: Arc<RwLock<VecDeque<ReplayEntry>>>,
    /// Maximum number of events to keep in the replay buffer.
    max_replay: usize,
    /// Maximum total approximate bytes in the replay buffer. 0 = unbounded.
    /// Note: this caps the replay buffer only. Live SSE subscribers still
    /// receive full, unmodified events via the broadcast channel.
    max_replay_bytes: usize,
    /// Current running byte total of entries in the replay buffer.
    current_bytes: Arc<RwLock<usize>>,
}

/// Cheap event-size estimate via JSON serialization. Returns a conservative
/// default on serialization failure so misconfigured events still count toward
/// the budget.
fn estimate_event_size(event: &CrewEvent) -> usize {
    serde_json::to_string(event).map(|s| s.len()).unwrap_or(256)
}

impl EventBus {
    pub fn new(capacity: usize) -> Self {
        let (sender, _) = broadcast::channel(capacity);
        let max_replay: usize = std::env::var("IRONCREW_MAX_EVENTS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(1000);
        // 4 MB default. 0 disables the byte cap.
        let max_replay_bytes: usize = std::env::var("IRONCREW_EVENT_REPLAY_MAX_BYTES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(4 * 1024 * 1024);
        Self {
            sender: Arc::new(sender),
            history: Arc::new(RwLock::new(VecDeque::with_capacity(max_replay.min(2048)))),
            max_replay,
            max_replay_bytes,
            current_bytes: Arc::new(RwLock::new(0)),
        }
    }

    pub fn emit(&self, event: CrewEvent) {
        let event = Arc::new(event);
        let size = estimate_event_size(&event);
        // Store in replay buffer, enforcing both count and byte budgets.
        // Live subscribers receive the full event below regardless of replay
        // buffer pressure — broadcasts are always lossless.
        if let (Ok(mut history), Ok(mut current_bytes)) =
            (self.history.try_write(), self.current_bytes.try_write())
        {
            // Evict oldest entries until both count and byte budgets are under.
            while history.len() >= self.max_replay
                || (self.max_replay_bytes > 0
                    && *current_bytes + size > self.max_replay_bytes
                    && !history.is_empty())
            {
                if let Some((_, evicted_size)) = history.pop_front() {
                    *current_bytes = current_bytes.saturating_sub(evicted_size);
                } else {
                    break;
                }
            }
            history.push_back((Arc::clone(&event), size));
            *current_bytes += size;
        }
        // Broadcast to live subscribers (ignore if none)
        let _ = self.sender.send(event);
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Arc<CrewEvent>> {
        self.sender.subscribe()
    }

    /// Get all events emitted so far (for replay to late subscribers).
    /// Returns Arc-wrapped events to avoid deep cloning.
    pub async fn replay(&self) -> Vec<Arc<CrewEvent>> {
        self.history
            .read()
            .await
            .iter()
            .map(|(e, _)| Arc::clone(e))
            .collect()
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new(256)
    }
}

impl EventBus {
    /// Test-only constructor that takes explicit budgets instead of reading
    /// process-global env vars. Avoids env-var races when tests run in
    /// parallel.
    #[cfg(test)]
    pub(crate) fn new_for_test(
        channel_capacity: usize,
        max_replay: usize,
        max_replay_bytes: usize,
    ) -> Self {
        let (sender, _) = broadcast::channel(channel_capacity);
        Self {
            sender: Arc::new(sender),
            history: Arc::new(RwLock::new(VecDeque::with_capacity(max_replay.min(2048)))),
            max_replay,
            max_replay_bytes,
            current_bytes: Arc::new(RwLock::new(0)),
        }
    }
}

#[cfg(test)]
mod event_shape_tests {
    use super::*;

    #[test]
    fn agent_tool_events_serialize_with_expected_tags() {
        let started = CrewEvent::AgentToolStarted {
            caller: "coord".into(),
            callee: "researcher".into(),
            prompt: "find facts".into(),
        };
        let completed = CrewEvent::AgentToolCompleted {
            caller: "coord".into(),
            callee: "researcher".into(),
            duration_ms: 42,
            success: true,
        };
        let started_json = serde_json::to_value(&started).unwrap();
        assert_eq!(started_json["event"], "agent_tool_started");
        assert_eq!(started_json["data"]["caller"], "coord");
        assert_eq!(started_json["data"]["callee"], "researcher");
        assert_eq!(started_json["data"]["prompt"], "find facts");

        let completed_json = serde_json::to_value(&completed).unwrap();
        assert_eq!(completed_json["event"], "agent_tool_completed");
        assert_eq!(completed_json["data"]["caller"], "coord");
        assert_eq!(completed_json["data"]["callee"], "researcher");
        assert_eq!(completed_json["data"]["duration_ms"], 42);
        assert_eq!(completed_json["data"]["success"], true);
    }
}

#[cfg(test)]
mod replay_buffer_tests {
    use super::*;

    fn make_log(msg: &str) -> CrewEvent {
        CrewEvent::Log {
            level: "info".into(),
            message: msg.into(),
        }
    }

    #[tokio::test]
    async fn count_cap_evicts_oldest() {
        // Generous byte budget (4 MB); count cap at 3 — eviction kicks in by count.
        let bus = EventBus::new_for_test(16, 3, 4 * 1024 * 1024);
        for i in 0..5 {
            bus.emit(make_log(&format!("msg {}", i)));
        }
        let replay = bus.replay().await;
        assert_eq!(replay.len(), 3, "expected count cap to keep 3 events");
    }

    #[tokio::test]
    async fn byte_cap_evicts_oldest() {
        // Generous count (1000); tight byte budget (200) — eviction by bytes.
        let bus = EventBus::new_for_test(16, 1000, 200);
        for i in 0..10 {
            bus.emit(make_log(&format!("msg {}", i)));
        }
        let replay = bus.replay().await;
        assert!(
            replay.len() < 10,
            "byte cap failed to evict; got {} events",
            replay.len()
        );
    }

    #[tokio::test]
    async fn no_eviction_under_budget() {
        let bus = EventBus::new_for_test(16, 1000, 4 * 1024 * 1024);
        for i in 0..5 {
            bus.emit(make_log(&format!("msg {}", i)));
        }
        let replay = bus.replay().await;
        assert_eq!(replay.len(), 5);
    }

    #[tokio::test]
    async fn broadcast_remains_lossless_when_replay_is_capped() {
        // Tight replay cap, but live subscribers should still get every event.
        let bus = EventBus::new_for_test(100, 2, 100);
        let mut rx = bus.subscribe();

        for i in 0..5 {
            bus.emit(make_log(&format!("msg {}", i)));
        }

        // Live subscriber receives all 5 events, even though the replay
        // buffer evicted most of them.
        let mut received = 0;
        while let Ok(_ev) = rx.try_recv() {
            received += 1;
        }
        assert_eq!(received, 5, "live subscriber should receive all events");

        // Replay buffer is capped
        let replay = bus.replay().await;
        assert!(replay.len() < 5, "replay buffer should be capped");
    }
}
