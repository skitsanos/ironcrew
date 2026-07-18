use serde::Serialize;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard};
use tokio::sync::broadcast;

const DEFAULT_EVENT_MAX_BYTES: usize = 256 * 1024;
const HARD_EVENT_MAX_BYTES: usize = 16 * 1024 * 1024;
const DEFAULT_REPLAY_MAX_EVENTS: usize = 1_000;
const HARD_REPLAY_MAX_EVENTS: usize = 10_000;
const DEFAULT_REPLAY_MAX_BYTES: usize = 4 * 1024 * 1024;
const HARD_REPLAY_MAX_BYTES: usize = 64 * 1024 * 1024;
const DEFAULT_LIVE_CHANNEL_CAPACITY: usize = 32;
const HARD_LIVE_CHANNEL_CAPACITY: usize = 256;
const TRUNCATION_MARKER: &str = "... [truncated]";

fn bounded_env(name: &str, default: usize, min: usize, max: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value >= min)
        .map(|value| value.min(max))
        .unwrap_or(default)
}

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

    // ─── Human-in-the-loop (crew:ask_human) ─────────────────────────────────
    #[serde(rename = "human_input_requested")]
    HumanInputRequested {
        question_id: String,
        prompt: String,
        choices: Vec<String>,
        timeout_s: u64,
        /// `"question"` (ask_human) or `"approval"` (tool approval gate).
        kind: String,
    },

    /// Deliberately carries no answer content: answers may contain secrets,
    /// and events land in the SSE replay buffer. The UI that answered
    /// already has the value.
    #[serde(rename = "human_input_received")]
    HumanInputReceived {
        question_id: String,
        /// `"answered"` or `"timeout"`.
        outcome: String,
    },

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

#[derive(Default)]
struct ReplayState {
    history: VecDeque<ReplayEntry>,
    current_bytes: usize,
}

#[derive(Clone)]
pub struct EventBus {
    sender: Arc<broadcast::Sender<Arc<CrewEvent>>>,
    /// Replay buffer: emitted events stored for late subscribers (capped).
    /// Each entry pairs the event with its approximate serialized size so the
    /// byte budget can be enforced without re-serializing on eviction.
    history: Arc<Mutex<ReplayState>>,
    /// Maximum number of events to keep in the replay buffer.
    max_replay: usize,
    /// Maximum total approximate bytes in the replay buffer.
    /// Individual live and replay events are separately bounded by
    /// `IRONCREW_EVENT_MAX_BYTES` before entering either channel.
    max_replay_bytes: usize,
    /// Maximum serialized size of one live/replay event.
    max_event_bytes: usize,
}

/// Event-size estimate via a counting serializer, avoiding a second allocation
/// proportional to an already-oversized event.
fn estimate_event_size(event: &CrewEvent) -> usize {
    struct Counter(usize);
    impl std::io::Write for Counter {
        fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
            self.0 = self.0.saturating_add(buffer.len());
            Ok(buffer.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    let mut counter = Counter(0);
    serde_json::to_writer(&mut counter, event)
        .map(|()| counter.0)
        .unwrap_or(256)
}

fn truncate_event_field(value: &mut String, max_bytes: usize) {
    if value.len() <= max_bytes {
        return;
    }
    let marker_bytes = TRUNCATION_MARKER.len().min(max_bytes);
    let mut boundary = max_bytes.saturating_sub(marker_bytes);
    while boundary > 0 && !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    value.truncate(boundary);
    value.push_str(&TRUNCATION_MARKER[..marker_bytes]);
    value.shrink_to_fit();
}

/// Bound the live event as well as replay retention. If truncating the event's
/// payload fields cannot bring it below the cap (for example, thousands of
/// oversized labels), replace it with a small warning event.
fn bound_event(mut event: CrewEvent, max_bytes: usize) -> CrewEvent {
    if estimate_event_size(&event) <= max_bytes {
        return event;
    }

    let field_budget = (max_bytes / 3).max(1);
    match &mut event {
        CrewEvent::CrewStarted { goal, .. } => truncate_event_field(goal, field_budget),
        CrewEvent::TaskCompleted { output, .. } => truncate_event_field(output, field_budget),
        CrewEvent::TaskFailed { error, .. }
        | CrewEvent::TaskRetry { error, .. }
        | CrewEvent::TaskThinking { content: error, .. }
        | CrewEvent::CollaborationTurn { content: error, .. }
        | CrewEvent::ConversationThinking { content: error, .. }
        | CrewEvent::DialogTurn { content: error, .. }
        | CrewEvent::DialogThinking { content: error, .. }
        | CrewEvent::Log { message: error, .. } => truncate_event_field(error, field_budget),
        CrewEvent::TaskSkipped { reason, .. } => truncate_event_field(reason, field_budget),
        CrewEvent::AgentToolStarted { prompt, .. } => truncate_event_field(prompt, field_budget),
        CrewEvent::ConversationTurn {
            user_message,
            assistant_message,
            ..
        } => {
            truncate_event_field(user_message, field_budget);
            truncate_event_field(assistant_message, field_budget);
        }
        CrewEvent::DialogCompleted { stop_reason, .. } => {
            if let Some(reason) = stop_reason {
                truncate_event_field(reason, field_budget);
            }
        }
        CrewEvent::HumanInputRequested {
            prompt, choices, ..
        } => {
            truncate_event_field(prompt, field_budget);
            let per_choice_budget = field_budget / choices.len().max(1);
            for choice in choices.iter_mut() {
                truncate_event_field(choice, per_choice_budget);
            }
        }
        CrewEvent::PhaseStart { .. }
        | CrewEvent::TaskAssigned { .. }
        | CrewEvent::ToolCall { .. }
        | CrewEvent::ToolResult { .. }
        | CrewEvent::AgentToolCompleted { .. }
        | CrewEvent::MessageSent { .. }
        | CrewEvent::ConversationStarted { .. }
        | CrewEvent::DialogStarted { .. }
        | CrewEvent::MemorySet { .. }
        | CrewEvent::HumanInputReceived { .. }
        | CrewEvent::RunComplete { .. } => {}
    }

    if estimate_event_size(&event) > max_bytes {
        CrewEvent::Log {
            level: "warn".into(),
            message: format!(
                "Event omitted because it exceeded IRONCREW_EVENT_MAX_BYTES ({max_bytes})"
            ),
        }
    } else {
        event
    }
}

impl EventBus {
    pub fn new(capacity: usize) -> Self {
        let max_replay = bounded_env(
            "IRONCREW_MAX_EVENTS",
            DEFAULT_REPLAY_MAX_EVENTS,
            1,
            HARD_REPLAY_MAX_EVENTS,
        );
        let max_replay_bytes = bounded_env(
            "IRONCREW_EVENT_REPLAY_MAX_BYTES",
            DEFAULT_REPLAY_MAX_BYTES,
            1024,
            HARD_REPLAY_MAX_BYTES,
        );
        let max_event_bytes = bounded_env(
            "IRONCREW_EVENT_MAX_BYTES",
            DEFAULT_EVENT_MAX_BYTES,
            1024,
            HARD_EVENT_MAX_BYTES,
        );
        let configured_capacity = bounded_env(
            "IRONCREW_EVENT_CHANNEL_CAPACITY",
            DEFAULT_LIVE_CHANNEL_CAPACITY,
            1,
            HARD_LIVE_CHANNEL_CAPACITY,
        );
        // Bound worst-case live-ring payloads to the replay byte budget. Slow
        // subscribers receive a Lagged warning instead of retaining hundreds
        // of maximum-sized events per active run/conversation.
        let byte_budget_capacity = (max_replay_bytes / max_event_bytes).max(1);
        let channel_capacity = capacity
            .max(1)
            .min(configured_capacity)
            .min(byte_budget_capacity);
        let (sender, _) = broadcast::channel(channel_capacity);
        Self {
            sender: Arc::new(sender),
            history: Arc::new(Mutex::new(ReplayState {
                history: VecDeque::with_capacity(max_replay.min(2048)),
                current_bytes: 0,
            })),
            max_replay,
            max_replay_bytes,
            max_event_bytes,
        }
    }

    fn replay_state(&self) -> MutexGuard<'_, ReplayState> {
        self.history
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub fn emit(&self, event: CrewEvent) {
        let event = bound_event(event, self.max_event_bytes);
        let event = Arc::new(event);
        let size = estimate_event_size(&event);
        // Mutating replay history and publishing to the broadcast channel are
        // one critical section. `subscribe_with_replay` takes the same lock,
        // so an event is always observed either in its snapshot or through the
        // receiver created under that lock -- never lost between the two.
        let mut state = self.replay_state();
        let fits_byte_budget = size <= self.max_replay_bytes;
        if fits_byte_budget {
            while state.history.len() >= self.max_replay
                || state.current_bytes.saturating_add(size) > self.max_replay_bytes
            {
                if let Some((_, evicted_size)) = state.history.pop_front() {
                    state.current_bytes = state.current_bytes.saturating_sub(evicted_size);
                }
            }
            state.history.push_back((Arc::clone(&event), size));
            state.current_bytes += size;
        }
        // Broadcast to live subscribers (ignore if none). This deliberately
        // occurs before releasing the replay lock; see the atomicity note.
        let _ = self.sender.send(event);
    }

    /// Atomically subscribe for future events and snapshot replay history.
    ///
    /// Callers must use this instead of separate `replay()` / `subscribe()`
    /// calls, which have an unavoidable gap where an emitted event can be
    /// absent from both the snapshot and the new receiver.
    pub fn subscribe_with_replay(
        &self,
    ) -> (Vec<Arc<CrewEvent>>, broadcast::Receiver<Arc<CrewEvent>>) {
        let state = self.replay_state();
        let receiver = self.sender.subscribe();
        let replay = state
            .history
            .iter()
            .map(|(event, _)| Arc::clone(event))
            .collect();
        (replay, receiver)
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
            history: Arc::new(Mutex::new(ReplayState {
                history: VecDeque::with_capacity(max_replay.min(2048)),
                current_bytes: 0,
            })),
            max_replay,
            max_replay_bytes,
            max_event_bytes: DEFAULT_EVENT_MAX_BYTES,
        }
    }
}

#[cfg(test)]
mod event_shape_tests {
    use super::*;

    #[test]
    fn oversized_unicode_event_is_bounded_before_broadcast() {
        let event = CrewEvent::TaskCompleted {
            task: "task".into(),
            agent: "agent".into(),
            duration_ms: 1,
            success: true,
            output: "🦀".repeat(100_000),
            token_usage: None,
        };
        let bounded = bound_event(event, 4096);
        assert!(estimate_event_size(&bounded) <= 4096);
        if let CrewEvent::TaskCompleted { output, .. } = bounded {
            assert!(std::str::from_utf8(output.as_bytes()).is_ok());
        }
    }

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
        let replay = bus.subscribe_with_replay().0;
        assert_eq!(replay.len(), 3, "expected count cap to keep 3 events");
    }

    #[tokio::test]
    async fn byte_cap_evicts_oldest() {
        // Generous count (1000); tight byte budget (200) — eviction by bytes.
        let bus = EventBus::new_for_test(16, 1000, 200);
        for i in 0..10 {
            bus.emit(make_log(&format!("msg {}", i)));
        }
        let replay = bus.subscribe_with_replay().0;
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
        let replay = bus.subscribe_with_replay().0;
        assert_eq!(replay.len(), 5);
    }

    #[tokio::test]
    async fn broadcast_remains_lossless_when_replay_is_capped() {
        // Tight replay cap, but live subscribers should still get every event.
        let bus = EventBus::new_for_test(100, 2, 100);
        let (_, mut rx) = bus.subscribe_with_replay();

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
        let replay = bus.subscribe_with_replay().0;
        assert!(replay.len() < 5, "replay buffer should be capped");
    }

    #[tokio::test]
    async fn atomic_subscription_never_loses_boundary_event() {
        for iteration in 0..100 {
            let bus = EventBus::new_for_test(16, 16, 4096);
            let emitter_bus = bus.clone();
            let barrier = Arc::new(std::sync::Barrier::new(2));
            let emitter_barrier = barrier.clone();
            let emitter = std::thread::spawn(move || {
                emitter_barrier.wait();
                emitter_bus.emit(make_log("boundary"));
            });

            barrier.wait();
            let (replay, mut receiver) = bus.subscribe_with_replay();
            emitter.join().unwrap();
            let observed = replay.len() + usize::from(receiver.try_recv().is_ok());
            assert_eq!(
                observed, 1,
                "boundary event was lost or duplicated on iteration {iteration}"
            );
        }
    }

    #[tokio::test]
    async fn event_larger_than_byte_budget_is_not_replayed() {
        let bus = EventBus::new_for_test(16, 16, 64);
        let (_, mut receiver) = bus.subscribe_with_replay();
        bus.emit(make_log(&"x".repeat(1024)));

        assert!(bus.subscribe_with_replay().0.is_empty());
        assert!(
            receiver.try_recv().is_ok(),
            "live delivery remains available"
        );
    }
}
