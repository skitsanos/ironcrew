use serde::Serialize;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, broadcast, mpsc, oneshot};

use crate::engine::run_events::{
    EventJournalScope, RunEventAppendBatch, RunEventAppendEntry, RunEventJournalConfig,
};
use crate::engine::store::StateStore;
use crate::utils::error::IronCrewError;

const DEFAULT_EVENT_MAX_BYTES: usize = 256 * 1024;
const HARD_EVENT_MAX_BYTES: usize = 16 * 1024 * 1024;
const DEFAULT_REPLAY_MAX_EVENTS: usize = 1_000;
const HARD_REPLAY_MAX_EVENTS: usize = 10_000;
const DEFAULT_REPLAY_MAX_BYTES: usize = 4 * 1024 * 1024;
const HARD_REPLAY_MAX_BYTES: usize = 64 * 1024 * 1024;
const DEFAULT_LIVE_CHANNEL_CAPACITY: usize = 32;
const HARD_LIVE_CHANNEL_CAPACITY: usize = 256;
const DEFAULT_DURABLE_QUEUE_MAX_EVENTS: usize = 64;
const DEFAULT_DURABLE_QUEUE_MAX_BYTES: usize = 1024 * 1024;
const DEFAULT_DURABLE_BATCH_MAX_EVENTS: usize = 32;
const DURABLE_APPEND_MAX_ATTEMPTS: usize = 3;
const DURABLE_APPEND_ATTEMPT_TIMEOUT: Duration = Duration::from_millis(1_500);
const DURABLE_TERMINAL_ACK_TIMEOUT: Duration = Duration::from_secs(5);
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

impl CrewEvent {
    /// Stable wire name shared by live SSE and the durable run-event journal.
    pub fn event_type(&self) -> &'static str {
        match self {
            Self::CrewStarted { .. } => "crew_started",
            Self::PhaseStart { .. } => "phase_start",
            Self::TaskAssigned { .. } => "task_assigned",
            Self::TaskCompleted { .. } => "task_completed",
            Self::TaskFailed { .. } => "task_failed",
            Self::TaskSkipped { .. } => "task_skipped",
            Self::TaskThinking { .. } => "task_thinking",
            Self::TaskRetry { .. } => "task_retry",
            Self::ToolCall { .. } => "tool_call",
            Self::ToolResult { .. } => "tool_result",
            Self::AgentToolStarted { .. } => "agent_tool_started",
            Self::AgentToolCompleted { .. } => "agent_tool_completed",
            Self::MessageSent { .. } => "message_sent",
            Self::CollaborationTurn { .. } => "collaboration_turn",
            Self::ConversationStarted { .. } => "conversation_started",
            Self::ConversationTurn { .. } => "conversation_turn",
            Self::ConversationThinking { .. } => "conversation_thinking",
            Self::DialogStarted { .. } => "dialog_started",
            Self::DialogTurn { .. } => "dialog_turn",
            Self::DialogThinking { .. } => "dialog_thinking",
            Self::DialogCompleted { .. } => "dialog_completed",
            Self::MemorySet { .. } => "memory_set",
            Self::HumanInputRequested { .. } => "human_input_requested",
            Self::HumanInputReceived { .. } => "human_input_received",
            Self::Log { .. } => "log",
            Self::RunComplete { .. } => "run_complete",
        }
    }
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

/// Result of waiting for a terminal event's best-effort durable append.
/// Local publication always happens before this result is returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use = "terminal journal persistence must be checked"]
pub enum DurableEventPersistence {
    /// The selected store has no cross-process event journal.
    NotConfigured,
    /// The shared journal acknowledged this event (or an identical retry).
    Persisted,
    /// A bounded producer queue could not accept the event.
    Dropped,
    /// The writer rejected the append without a retryable outcome.
    Failed,
    /// Queueing or persistence did not finish within the terminal deadline.
    TimedOut,
}

struct PreparedDurableEvent {
    event_type: String,
    payload: serde_json::Value,
    payload_bytes: usize,
}

struct DurableEnvelope {
    entry: RunEventAppendEntry,
    _byte_permit: OwnedSemaphorePermit,
    acknowledgement: Option<oneshot::Sender<DurableEventPersistence>>,
}

enum DurableCommand {
    Event(DurableEnvelope),
    Flush(oneshot::Sender<DurableEventPersistence>),
}

struct DurableCompletion {
    _byte_permit: OwnedSemaphorePermit,
    acknowledgement: Option<oneshot::Sender<DurableEventPersistence>>,
}

struct ReservedTerminalEvent {
    entry: RunEventAppendEntry,
}

struct DurableSequenceState {
    next_sequence: u64,
    terminal_sealed: bool,
    flush_pending: bool,
}

struct DurableProducer {
    sender: mpsc::Sender<DurableCommand>,
    byte_slots: Arc<Semaphore>,
    sequence: Mutex<DurableSequenceState>,
    dropped_events: AtomicU64,
    config: RunEventJournalConfig,
    flow: String,
    run_id: String,
}

impl DurableProducer {
    fn start(store: Arc<dyn StateStore>, flow: String, run_id: String) -> Option<Arc<Self>> {
        let config = store.event_journal_config();
        if let Err(error) = config.validate() {
            tracing::warn!(
                run_id = %run_id,
                error_kind = durable_error_kind(&error),
                "Durable run-event producer disabled because journal limits are invalid"
            );
            return None;
        }

        let handle = match tokio::runtime::Handle::try_current() {
            Ok(handle) => handle,
            Err(_) => {
                tracing::warn!(
                    run_id = %run_id,
                    "Durable run-event producer requires an active Tokio runtime"
                );
                return None;
            }
        };
        let queue_max_events = config
            .max_events_per_run
            .clamp(1, DEFAULT_DURABLE_QUEUE_MAX_EVENTS);
        // The default is one MiB per active run. An explicitly raised
        // single-event limit can enlarge this enough to admit one event, but
        // the queue never exceeds the configured per-run journal budget.
        let queue_max_bytes = DEFAULT_DURABLE_QUEUE_MAX_BYTES
            .max(config.max_event_bytes)
            .min(config.max_bytes_per_run);
        let (sender, receiver) = mpsc::channel(queue_max_events);
        let producer = Arc::new(Self {
            sender,
            byte_slots: Arc::new(Semaphore::new(queue_max_bytes)),
            sequence: Mutex::new(DurableSequenceState {
                next_sequence: 1,
                terminal_sealed: false,
                flush_pending: false,
            }),
            dropped_events: AtomicU64::new(0),
            config: config.clone(),
            flow: flow.clone(),
            run_id: run_id.clone(),
        });

        let owner_instance_id = store.instance_id().to_string();
        handle.spawn(durable_event_writer(
            store,
            receiver,
            config,
            flow,
            run_id,
            owner_instance_id,
        ));
        Some(producer)
    }

    fn prepare(&self, event: &CrewEvent) -> Option<PreparedDurableEvent> {
        let payload = match event {
            CrewEvent::HumanInputRequested {
                question_id,
                timeout_s,
                kind,
                ..
            } => serde_json::json!({
                "event": event.event_type(),
                "data": {
                    "question_id": question_id,
                    "timeout_s": timeout_s,
                    "kind": kind,
                    "question_method": "GET",
                    "question_endpoint": format!(
                        "/flows/{}/questions/{}",
                        self.flow, self.run_id
                    ),
                    "question_metadata": "omitted_from_event_journal"
                }
            }),
            _ => match serde_json::to_value(event) {
                Ok(payload) => payload,
                Err(_) => return None,
            },
        };
        let payload_bytes = serialized_size(&payload)?;
        Some(PreparedDurableEvent {
            event_type: event.event_type().to_string(),
            payload,
            payload_bytes,
        })
    }

    /// Called while the EventBus replay lock is held, preserving the same
    /// total ordering for local publication and durable sequence allocation.
    fn enqueue(&self, prepared: Option<PreparedDurableEvent>) {
        let mut state = self
            .sequence
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.terminal_sealed {
            drop(state);
            self.record_drop("after_terminal");
            return;
        }
        let Some(sequence) = allocate_sequence(&mut state) else {
            drop(state);
            self.record_drop("sequence_exhausted");
            return;
        };
        if state.flush_pending {
            drop(state);
            self.record_drop("flush_barrier");
            return;
        }
        let Some(prepared) = prepared else {
            drop(state);
            self.record_drop("serialization");
            return;
        };
        let prepared_payload_bytes = prepared.payload_bytes;
        let Ok(entry) = RunEventAppendEntry::new(
            sequence,
            prepared.event_type,
            prepared.payload,
            self.config.max_event_bytes,
        ) else {
            drop(state);
            self.record_drop("validation");
            return;
        };
        if entry.payload_bytes != prepared_payload_bytes {
            drop(state);
            self.record_drop("serialization");
            return;
        }
        let Ok(permit_count) = u32::try_from(prepared_payload_bytes) else {
            drop(state);
            self.record_drop("byte_budget");
            return;
        };
        let Ok(byte_permit) = Arc::clone(&self.byte_slots).try_acquire_many_owned(permit_count)
        else {
            drop(state);
            self.record_drop("byte_budget");
            return;
        };
        let envelope = DurableEnvelope {
            entry,
            _byte_permit: byte_permit,
            acknowledgement: None,
        };
        if self
            .sender
            .try_send(DurableCommand::Event(envelope))
            .is_err()
        {
            drop(state);
            self.record_drop("count_budget");
        }
    }

    /// Reserve a FIFO flush barrier while the EventBus replay lock is held.
    /// Regular sync emitters cannot overtake a barrier that is waiting for
    /// bounded channel capacity.
    fn reserve_flush(&self) -> bool {
        let mut state = self
            .sequence
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.flush_pending {
            return false;
        }
        state.flush_pending = true;
        true
    }

    async fn flush(&self) -> DurableEventPersistence {
        let mut barrier = FlushBarrierGuard::new(self);
        let deadline = tokio::time::Instant::now() + DURABLE_TERMINAL_ACK_TIMEOUT;
        let sender_permit = match tokio::time::timeout_at(deadline, self.sender.reserve()).await {
            Ok(Ok(permit)) => permit,
            Ok(Err(_)) => return DurableEventPersistence::Failed,
            Err(_) => return DurableEventPersistence::TimedOut,
        };
        let (acknowledgement, receiver) = oneshot::channel();
        sender_permit.send(DurableCommand::Flush(acknowledgement));
        barrier.release();

        match tokio::time::timeout_at(deadline, receiver).await {
            Ok(Ok(outcome)) => outcome,
            Ok(Err(_)) => DurableEventPersistence::Failed,
            Err(_) => DurableEventPersistence::TimedOut,
        }
    }

    fn clear_flush_barrier(&self) {
        self.sequence
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .flush_pending = false;
    }

    /// Reserve the terminal sequence synchronously while the replay lock is
    /// held. Reserving it permanently seals the durable stream; later local
    /// emissions cannot overtake or follow `run_complete` in the journal.
    fn reserve_terminal(
        &self,
        prepared: Option<PreparedDurableEvent>,
    ) -> Option<ReservedTerminalEvent> {
        let mut state = self
            .sequence
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.flush_pending {
            state.terminal_sealed = true;
            drop(state);
            self.record_drop("flush_barrier");
            return None;
        }
        if state.terminal_sealed {
            drop(state);
            self.record_drop("after_terminal");
            return None;
        }
        let Some(sequence) = allocate_sequence(&mut state) else {
            drop(state);
            self.record_drop("sequence_exhausted");
            return None;
        };
        // Local terminal publication is authoritative even if serialization
        // or durable queueing fails. Keep the durable stream sealed so the
        // store can report a terminal run with no terminal journal row as
        // incomplete instead of later accepting events after completion.
        state.terminal_sealed = true;
        let Some(prepared) = prepared else {
            drop(state);
            self.record_drop("serialization");
            return None;
        };
        let prepared_payload_bytes = prepared.payload_bytes;
        let entry = match RunEventAppendEntry::new(
            sequence,
            prepared.event_type,
            prepared.payload,
            self.config.max_event_bytes,
        ) {
            Ok(entry) => entry,
            Err(_) => {
                drop(state);
                self.record_drop("validation");
                return None;
            }
        };
        if entry.payload_bytes != prepared_payload_bytes {
            drop(state);
            self.record_drop("serialization");
            return None;
        }
        Some(ReservedTerminalEvent { entry })
    }

    async fn enqueue_terminal(&self, reserved: ReservedTerminalEvent) -> DurableEventPersistence {
        let deadline = tokio::time::Instant::now() + DURABLE_TERMINAL_ACK_TIMEOUT;
        let permit_count = match u32::try_from(reserved.entry.payload_bytes) {
            Ok(permit_count) => permit_count,
            Err(_) => {
                self.record_drop("byte_budget");
                return DurableEventPersistence::Dropped;
            }
        };
        let byte_permit = match tokio::time::timeout_at(
            deadline,
            Arc::clone(&self.byte_slots).acquire_many_owned(permit_count),
        )
        .await
        {
            Ok(Ok(permit)) => permit,
            Ok(Err(_)) => return DurableEventPersistence::Failed,
            Err(_) => return DurableEventPersistence::TimedOut,
        };
        let sender_permit = match tokio::time::timeout_at(deadline, self.sender.reserve()).await {
            Ok(Ok(permit)) => permit,
            Ok(Err(_)) => return DurableEventPersistence::Failed,
            Err(_) => return DurableEventPersistence::TimedOut,
        };
        let (acknowledgement, receiver) = oneshot::channel();
        sender_permit.send(DurableCommand::Event(DurableEnvelope {
            entry: reserved.entry,
            _byte_permit: byte_permit,
            acknowledgement: Some(acknowledgement),
        }));

        match tokio::time::timeout_at(deadline, receiver).await {
            Ok(Ok(outcome)) => outcome,
            Ok(Err(_)) => DurableEventPersistence::Failed,
            Err(_) => DurableEventPersistence::TimedOut,
        }
    }

    fn record_drop(&self, reason: &'static str) {
        let count = self
            .dropped_events
            .fetch_add(1, Ordering::Relaxed)
            .saturating_add(1);
        if count.is_power_of_two() {
            tracing::warn!(
                run_id = %self.run_id,
                dropped_events = count,
                reason,
                "Durable run-event producer omitted an event from shared replay"
            );
        }
    }

    #[cfg(test)]
    fn new_unspawned_for_test(
        config: RunEventJournalConfig,
        flow: &str,
        run_id: &str,
        queue_max_events: usize,
        queue_max_bytes: usize,
    ) -> (Arc<Self>, mpsc::Receiver<DurableCommand>) {
        let (sender, receiver) = mpsc::channel(queue_max_events);
        (
            Arc::new(Self {
                sender,
                byte_slots: Arc::new(Semaphore::new(queue_max_bytes)),
                sequence: Mutex::new(DurableSequenceState {
                    next_sequence: 1,
                    terminal_sealed: false,
                    flush_pending: false,
                }),
                dropped_events: AtomicU64::new(0),
                config,
                flow: flow.to_string(),
                run_id: run_id.to_string(),
            }),
            receiver,
        )
    }
}

struct FlushBarrierGuard<'a> {
    producer: &'a DurableProducer,
    armed: bool,
}

impl<'a> FlushBarrierGuard<'a> {
    fn new(producer: &'a DurableProducer) -> Self {
        Self {
            producer,
            armed: true,
        }
    }

    fn release(&mut self) {
        if self.armed {
            self.producer.clear_flush_barrier();
            self.armed = false;
        }
    }
}

impl Drop for FlushBarrierGuard<'_> {
    fn drop(&mut self) {
        self.release();
    }
}

fn allocate_sequence(state: &mut DurableSequenceState) -> Option<u64> {
    let sequence = state.next_sequence;
    state.next_sequence = sequence.checked_add(1)?;
    Some(sequence)
}

fn serialized_size<T: Serialize + ?Sized>(value: &T) -> Option<usize> {
    struct Counter(usize);

    impl std::io::Write for Counter {
        fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
            self.0 = self
                .0
                .checked_add(buffer.len())
                .ok_or_else(|| std::io::Error::other("serialized run-event size exceeds usize"))?;
            Ok(buffer.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    let mut counter = Counter(0);
    serde_json::to_writer(&mut counter, value).ok()?;
    Some(counter.0)
}

async fn durable_event_writer(
    store: Arc<dyn StateStore>,
    mut receiver: mpsc::Receiver<DurableCommand>,
    config: RunEventJournalConfig,
    flow: String,
    run_id: String,
    owner_instance_id: String,
) {
    let batch_max_events = config
        .page_max_events
        .clamp(1, DEFAULT_DURABLE_BATCH_MAX_EVENTS);
    let batch_max_bytes = config.page_max_bytes.min(config.max_bytes_per_run);
    let mut carried = None;
    let mut writer_failed = false;

    loop {
        let command = match carried.take() {
            Some(command) => command,
            None => match receiver.recv().await {
                Some(command) => command,
                None => break,
            },
        };
        let first = match command {
            DurableCommand::Event(envelope) => envelope,
            DurableCommand::Flush(acknowledgement) => {
                let outcome = if writer_failed {
                    DurableEventPersistence::Failed
                } else {
                    DurableEventPersistence::Persisted
                };
                let _ = acknowledgement.send(outcome);
                continue;
            }
        };
        let mut entries = Vec::with_capacity(batch_max_events);
        let mut completions = Vec::with_capacity(batch_max_events);
        let mut batch_bytes = 0usize;
        push_durable_envelope(first, &mut entries, &mut completions, &mut batch_bytes);

        while entries.len() < batch_max_events {
            match receiver.try_recv() {
                Ok(DurableCommand::Event(envelope))
                    if batch_bytes.saturating_add(envelope.entry.payload_bytes)
                        <= batch_max_bytes =>
                {
                    push_durable_envelope(
                        envelope,
                        &mut entries,
                        &mut completions,
                        &mut batch_bytes,
                    );
                }
                Ok(command) => {
                    carried = Some(command);
                    break;
                }
                Err(mpsc::error::TryRecvError::Empty) => break,
                Err(mpsc::error::TryRecvError::Disconnected) => break,
            }
        }

        let batch = RunEventAppendBatch {
            run_id: run_id.clone(),
            flow: flow.clone(),
            owner_instance_id: owner_instance_id.clone(),
            entries,
        };
        let terminal_batch = batch
            .entries
            .iter()
            .any(|entry| entry.event_type == "run_complete");
        let outcome = if append_durable_batch_with_retries(&store, &batch).await {
            DurableEventPersistence::Persisted
        } else {
            writer_failed = true;
            DurableEventPersistence::Failed
        };
        for completion in completions {
            if let Some(acknowledgement) = completion.acknowledgement {
                let _ = acknowledgement.send(outcome);
            }
        }
        if terminal_batch {
            break;
        }
    }
}

fn push_durable_envelope(
    envelope: DurableEnvelope,
    entries: &mut Vec<RunEventAppendEntry>,
    completions: &mut Vec<DurableCompletion>,
    batch_bytes: &mut usize,
) {
    let DurableEnvelope {
        entry,
        _byte_permit,
        acknowledgement,
    } = envelope;
    *batch_bytes = batch_bytes.saturating_add(entry.payload_bytes);
    entries.push(entry);
    completions.push(DurableCompletion {
        _byte_permit,
        acknowledgement,
    });
}

async fn append_durable_batch_with_retries(
    store: &Arc<dyn StateStore>,
    batch: &RunEventAppendBatch,
) -> bool {
    for attempt in 1..=DURABLE_APPEND_MAX_ATTEMPTS {
        let append = tokio::time::timeout(
            DURABLE_APPEND_ATTEMPT_TIMEOUT,
            store.append_run_events(batch),
        )
        .await;
        match append {
            Ok(Ok(_)) => return true,
            Ok(Err(error)) => {
                let retryable = durable_append_error_is_retryable(&error);
                tracing::warn!(
                    run_id = %batch.run_id,
                    attempt,
                    max_attempts = DURABLE_APPEND_MAX_ATTEMPTS,
                    error_kind = durable_error_kind(&error),
                    retryable,
                    "Durable run-event append failed"
                );
                if !retryable || attempt == DURABLE_APPEND_MAX_ATTEMPTS {
                    return false;
                }
            }
            Err(_) => {
                tracing::warn!(
                    run_id = %batch.run_id,
                    attempt,
                    max_attempts = DURABLE_APPEND_MAX_ATTEMPTS,
                    "Durable run-event append timed out"
                );
                if attempt == DURABLE_APPEND_MAX_ATTEMPTS {
                    return false;
                }
            }
        }
        let backoff_ms = 50u64.saturating_mul(1u64 << (attempt - 1));
        tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
    }
    false
}

fn durable_append_error_is_retryable(error: &IronCrewError) -> bool {
    match error {
        IronCrewError::Io(_) | IronCrewError::Http(_) => true,
        IronCrewError::Validation(message) => {
            // The run worker can emit immediately before its run-intent row is
            // visible. PostgreSQL transport/pool failures currently also use
            // the validation envelope; both classes get only bounded retries.
            message.contains("not found") || message.starts_with("PostgreSQL run-event")
        }
        _ => false,
    }
}

fn durable_error_kind(error: &IronCrewError) -> &'static str {
    match error {
        IronCrewError::Io(_) => "io",
        IronCrewError::Http(_) => "http",
        IronCrewError::Validation(_) => "validation",
        IronCrewError::Conflict(_) => "conflict",
        _ => "application",
    }
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
    /// Present only for shared stores. Clones share one queue and writer.
    durable: Option<Arc<DurableProducer>>,
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
            durable: None,
        }
    }

    /// Enable bounded, cross-process replay when the selected store exposes a
    /// shared event journal. Process-local stores retain exactly the behavior
    /// of [`Self::new`]. This constructor must run inside a Tokio runtime.
    pub fn new_durable(
        capacity: usize,
        store: Arc<dyn StateStore>,
        flow: impl Into<String>,
        run_id: impl Into<String>,
    ) -> Self {
        let mut bus = Self::new(capacity);
        if store.event_journal_scope() == EventJournalScope::SharedStore {
            bus.durable = DurableProducer::start(store, flow.into(), run_id.into());
        }
        bus
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
        let durable_prepared = self
            .durable
            .as_ref()
            .and_then(|producer| producer.prepare(&event));
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
        // Sequence allocation and queue insertion happen under the same replay
        // lock, so concurrent emitters cannot reorder local and durable views.
        if let Some(producer) = &self.durable {
            producer.enqueue(durable_prepared);
        }
    }

    /// Wait for every durable writer command queued before this barrier.
    ///
    /// This is a local no-op. Shared-store callers use it before transitioning
    /// a run record to terminal status, ensuring the strict terminal fence
    /// cannot reject legitimate nonterminal events still in the writer queue.
    /// The wait, including queue admission and append acknowledgement, is
    /// bounded by the same five-second deadline as terminal publication.
    pub async fn flush_durable(&self) -> DurableEventPersistence {
        let Some(producer) = &self.durable else {
            return DurableEventPersistence::NotConfigured;
        };
        let reserved = {
            // Synchronize with regular emitters: everything locally published
            // before this lock acquisition has already entered the durable
            // FIFO or consumed a deliberate gap sequence.
            let _replay_guard = self.replay_state();
            producer.reserve_flush()
        };
        if !reserved {
            return DurableEventPersistence::Failed;
        }
        producer.flush().await
    }

    /// Publish a terminal event locally and wait for a bounded, best-effort
    /// durable acknowledgement. The HTTP monitor calls this only after the
    /// terminal run record is persisted, keeping terminal metadata ahead of
    /// the journal's `run_complete` row.
    pub async fn emit_terminal(&self, event: CrewEvent) -> DurableEventPersistence {
        let event = bound_event(event, self.max_event_bytes);
        let event = Arc::new(event);
        let size = estimate_event_size(&event);
        let durable_prepared = self
            .durable
            .as_ref()
            .and_then(|producer| producer.prepare(&event));
        let reserved = {
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
            let _ = self.sender.send(event);
            self.durable
                .as_ref()
                .and_then(|producer| producer.reserve_terminal(durable_prepared))
        };

        match (&self.durable, reserved) {
            (None, _) => DurableEventPersistence::NotConfigured,
            (Some(producer), Some(reserved)) => producer.enqueue_terminal(reserved).await,
            (Some(_), None) => DurableEventPersistence::Dropped,
        }
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
            durable: None,
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

#[cfg(test)]
mod durable_producer_tests {
    use super::*;

    fn test_bus(
        queue_max_events: usize,
        queue_max_bytes: usize,
    ) -> (
        EventBus,
        Arc<DurableProducer>,
        mpsc::Receiver<DurableCommand>,
    ) {
        let config = RunEventJournalConfig::default();
        let (producer, receiver) = DurableProducer::new_unspawned_for_test(
            config,
            "review-flow",
            "run-123",
            queue_max_events,
            queue_max_bytes,
        );
        let mut bus = EventBus::new_for_test(16, 16, 4 * 1024 * 1024);
        bus.durable = Some(Arc::clone(&producer));
        (bus, producer, receiver)
    }

    fn event_command(command: DurableCommand) -> DurableEnvelope {
        match command {
            DurableCommand::Event(envelope) => envelope,
            DurableCommand::Flush(_) => panic!("expected durable event command"),
        }
    }

    #[test]
    fn durable_human_input_event_omits_question_metadata() {
        let (bus, _, mut receiver) = test_bus(4, 1024 * 1024);
        bus.emit(CrewEvent::HumanInputRequested {
            question_id: "approval-1".into(),
            prompt: "deploy with secret-value?".into(),
            choices: vec!["secret-value".into(), "no".into()],
            timeout_s: 60,
            kind: "approval".into(),
        });

        let envelope = event_command(receiver.try_recv().expect("durable event"));
        assert_eq!(envelope.entry.sequence, 1);
        assert_eq!(envelope.entry.event_type, "human_input_requested");
        let data = &envelope.entry.payload["data"];
        assert_eq!(data["question_id"], "approval-1");
        assert_eq!(data["question_method"], "GET");
        assert_eq!(
            data["question_endpoint"],
            "/flows/review-flow/questions/run-123"
        );
        assert!(data.get("prompt").is_none());
        assert!(data.get("choices").is_none());
        let serialized = serde_json::to_string(&envelope.entry.payload).unwrap();
        assert!(!serialized.contains("secret-value"));
    }

    #[test]
    fn accepted_events_keep_strict_emission_order() {
        let (bus, _, mut receiver) = test_bus(4, 1024 * 1024);
        for message in ["one", "two", "three"] {
            bus.emit(make_durable_log(message));
        }

        for expected_sequence in 1..=3 {
            let envelope = event_command(receiver.try_recv().expect("ordered event"));
            assert_eq!(envelope.entry.sequence, expected_sequence);
        }
    }

    #[test]
    fn full_queue_creates_an_observable_sequence_gap() {
        let (bus, producer, mut receiver) = test_bus(1, 1024 * 1024);
        bus.emit(make_durable_log("one"));
        bus.emit(make_durable_log("two"));
        bus.emit(make_durable_log("three"));

        let first = event_command(receiver.try_recv().expect("first queued event"));
        assert_eq!(first.entry.sequence, 1);
        drop(first);
        bus.emit(make_durable_log("four"));
        let fourth = event_command(receiver.try_recv().expect("post-gap event"));
        assert_eq!(fourth.entry.sequence, 4);
        assert_eq!(producer.dropped_events.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn byte_budget_drop_also_consumes_a_sequence() {
        let (bus, producer, mut receiver) = test_bus(4, 96);
        assert!(
            producer
                .prepare(&make_durable_log(&"x".repeat(200)))
                .unwrap()
                .payload_bytes
                > 96
        );
        assert!(
            producer
                .prepare(&make_durable_log("ok"))
                .unwrap()
                .payload_bytes
                <= 96
        );

        bus.emit(make_durable_log(&"x".repeat(200)));
        bus.emit(make_durable_log("ok"));
        let retained = event_command(receiver.try_recv().expect("event after byte gap"));
        assert_eq!(retained.entry.sequence, 2);
        assert_eq!(producer.dropped_events.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn only_temporary_append_failures_are_retried() {
        assert!(durable_append_error_is_retryable(&IronCrewError::Io(
            std::io::Error::other("connection reset")
        )));
        assert!(durable_append_error_is_retryable(
            &IronCrewError::Validation("Run 'run-123' not found".into())
        ));
        assert!(durable_append_error_is_retryable(
            &IronCrewError::Validation("PostgreSQL run-event append transaction failed".into())
        ));
        assert!(!durable_append_error_is_retryable(
            &IronCrewError::Conflict("owner fence lost".into())
        ));
        assert!(!durable_append_error_is_retryable(
            &IronCrewError::Validation("invalid event type".into())
        ));
    }

    #[tokio::test]
    async fn durable_flush_is_a_bounded_fifo_barrier_without_sealing() {
        let (bus, producer, mut receiver) = test_bus(1, 1024 * 1024);
        bus.emit(make_durable_log("before-flush"));

        let flush_bus = bus.clone();
        let flush_task = tokio::spawn(async move { flush_bus.flush_durable().await });
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if producer
                    .sequence
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .flush_pending
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("flush reservation");

        // A synchronous emitter cannot overtake the pending FIFO barrier. It
        // consumes sequence 2 as a gap that the next persisted event exposes.
        bus.emit(make_durable_log("during-flush"));
        let before = event_command(receiver.recv().await.expect("pre-flush event"));
        assert_eq!(before.entry.sequence, 1);
        assert!(!flush_task.is_finished());
        drop(before);

        let acknowledgement = match receiver.recv().await.expect("flush command") {
            DurableCommand::Flush(acknowledgement) => acknowledgement,
            DurableCommand::Event(_) => panic!("flush was overtaken by an event"),
        };
        assert!(!flush_task.is_finished());
        acknowledgement
            .send(DurableEventPersistence::Persisted)
            .unwrap();
        assert_eq!(
            flush_task.await.unwrap(),
            DurableEventPersistence::Persisted
        );

        assert!(
            !producer
                .sequence
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .terminal_sealed
        );
        bus.emit(make_durable_log("after-flush"));
        let after = event_command(receiver.recv().await.expect("post-flush event"));
        assert_eq!(after.entry.sequence, 3);
    }

    #[tokio::test]
    async fn durable_flush_is_a_local_no_op() {
        assert_eq!(
            EventBus::new_for_test(4, 4, 4096).flush_durable().await,
            DurableEventPersistence::NotConfigured
        );
    }

    #[tokio::test]
    async fn terminal_emit_waits_for_queue_space_and_persistence_ack() {
        let (bus, producer, mut receiver) = test_bus(1, 1024 * 1024);
        bus.emit(make_durable_log("queued"));
        // This consumes sequence 2 but cannot enter the full queue. A
        // successful terminal row at sequence 3 makes the gap store-visible.
        bus.emit(make_durable_log("pre-terminal-drop"));

        let terminal_bus = bus.clone();
        let terminal_task = tokio::spawn(async move {
            terminal_bus
                .emit_terminal(CrewEvent::RunComplete {
                    run_id: "run-123".into(),
                    status: "completed".into(),
                    duration_ms: 10,
                    total_tokens: 20,
                })
                .await
        });
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if producer
                    .sequence
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .terminal_sealed
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("terminal reservation");

        // A later local event cannot overtake or follow the terminal event in
        // the durable journal. A sealed stream allocates no further sequence.
        bus.emit(make_durable_log("after-terminal"));
        let queued = event_command(receiver.recv().await.expect("initial queued event"));
        assert_eq!(queued.entry.sequence, 1);
        drop(queued);

        let mut terminal = event_command(receiver.recv().await.expect("terminal event"));
        assert_eq!(terminal.entry.sequence, 3);
        terminal
            .acknowledgement
            .take()
            .expect("terminal acknowledgement")
            .send(DurableEventPersistence::Persisted)
            .unwrap();
        drop(terminal);
        assert_eq!(
            terminal_task.await.unwrap(),
            DurableEventPersistence::Persisted
        );

        bus.emit(make_durable_log("after-ack"));
        assert!(receiver.try_recv().is_err());
        let sequence = producer
            .sequence
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .next_sequence;
        assert_eq!(sequence, 4, "sealed streams allocate no later sequences");
        assert_eq!(producer.dropped_events.load(Ordering::Relaxed), 3);
    }

    #[tokio::test]
    async fn terminal_writer_failure_is_returned_to_the_caller() {
        let (bus, producer, mut receiver) = test_bus(1, 1024 * 1024);
        let task = tokio::spawn(async move {
            bus.emit_terminal(CrewEvent::RunComplete {
                run_id: "run-123".into(),
                status: "failed".into(),
                duration_ms: 1,
                total_tokens: 0,
            })
            .await
        });
        let mut terminal = event_command(receiver.recv().await.expect("terminal event"));
        terminal
            .acknowledgement
            .take()
            .expect("terminal acknowledgement")
            .send(DurableEventPersistence::Failed)
            .unwrap();
        assert_eq!(task.await.unwrap(), DurableEventPersistence::Failed);
        assert!(
            producer
                .sequence
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .terminal_sealed,
            "a failed terminal append must still seal the producer"
        );
    }

    fn make_durable_log(message: &str) -> CrewEvent {
        CrewEvent::Log {
            level: "info".into(),
            message: message.into(),
        }
    }
}
