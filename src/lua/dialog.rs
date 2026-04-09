//! AgentDialog — agent-to-agent multi-turn conversation.
//!
//! Two agents take turns responding to each other with perspective-flipped
//! message histories. Each agent sees its own previous turns as `assistant`
//! messages and the opponent's turns as `user` messages, prefixed with the
//! opponent's name for context.
//!
//! Created via `crew:dialog({})`. Reuses the crew's provider, model, and tool
//! registry. Streams to stderr (with dim reasoning) and captures reasoning per
//! turn.
//!
//! ## Future work
//! - Cross-run persistence

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use mlua::{Table, UserData, UserDataMethods, Value};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::engine::agent::Agent;
use crate::engine::eventbus::{CrewEvent, EventBus};
use crate::engine::sessions::{DialogStateRecord, validate_session_id};
use crate::engine::store::StateStore;
use crate::llm::provider::{
    ChatMessage, ChatRequest, ChatResponse, LlmProvider, StreamChunk, ToolCallRequest,
};
use crate::tools::registry::ToolRegistry;
use crate::utils::error::IronCrewError;

/// Convert a 0-based agent index into a stable positional label
/// (`"a"`, `"b"`, ..., `"z"`). Used in SSE events for backward compatibility.
fn speaker_label(index: usize) -> String {
    if index < 26 {
        ((b'a' + index as u8) as char).to_string()
    } else {
        format!("agent_{}", index)
    }
}

/// One turn in the dialog transcript.
/// `speaker_index` is the position in the agents vec. The corresponding
/// agent name is `agent_name`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DialogTurn {
    pub index: usize,
    pub speaker_index: usize,
    pub agent_name: String,
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
}

/// State of an N-agent dialog (N >= 2). Agents take turns in round-robin
/// order starting from `starting_speaker`.
pub struct AgentDialog {
    /// Stable identifier — included in every SSE event for this dialog.
    pub id: String,

    /// Participants in turn order. Length must be >= 2.
    pub agents: Vec<Agent>,

    pub provider: Arc<dyn LlmProvider>,
    pub tool_registry: ToolRegistry,
    pub model: String,

    /// The kickoff message that the first speaker responds to.
    pub starter: String,

    pub max_turns: usize,
    pub max_history: Option<usize>,
    pub stream: bool,
    pub max_tool_rounds: usize,
    /// 0-based index into `agents` of the agent who speaks first.
    pub starting_speaker: usize,

    /// The shared transcript — turns in chronological order.
    /// Stored as a VecDeque so `max_history` trimming can pop from the front in O(1).
    pub transcript: Mutex<VecDeque<DialogTurn>>,
    /// Index of the next turn to run (0-based).
    pub next_index: Mutex<usize>,

    /// EventBus for emitting dialog_* SSE events.
    pub eventbus: EventBus,

    /// Tracks whether dialog_completed has been emitted (set to true after run_all
    /// reaches max_turns) so it isn't emitted twice for the same dialog.
    pub completed_emitted: Mutex<bool>,

    /// Optional Lua callback for selecting the next speaker.
    /// Signature: `function(transcript_table, agents_table) -> agent_name`
    /// Stored as a registry key for thread safety. When `None`, round-robin.
    pub turn_selector_key: Option<mlua::RegistryKey>,

    /// Optional Lua callback for custom early termination.
    ///
    /// Signature: `function(last_turn_table, transcript_table) -> bool | string | nil`
    ///
    /// Return values:
    ///   - `false` / `nil`  → continue
    ///   - `true`           → stop (reason = "custom_stop")
    ///   - `"reason"`       → stop with that reason string
    ///
    /// Stored as a registry key for thread safety. When `None`, the dialog
    /// runs until `max_turns` is reached.
    pub should_stop_key: Option<mlua::RegistryKey>,

    /// Set to true once a `should_stop` callback has requested termination.
    /// `has_turns_remaining` consults this flag so manual `next_turn` loops
    /// also respect the stop condition.
    pub stopped: Mutex<bool>,

    /// The reason the dialog stopped early, if any. `None` while running
    /// and after a natural `max_turns` completion.
    pub stop_reason: Mutex<Option<String>>,

    /// `true` when the caller supplied a stable `id` and the session is
    /// eligible for cross-run persistence.
    pub persistent: bool,

    /// Optional state store for cross-run persistence.
    pub store: Option<Arc<dyn StateStore>>,

    /// Flow label persisted alongside the session (taken from `crew.goal`).
    pub flow_name: String,

    /// When `true` (default for persistent sessions), the dialog auto-saves
    /// after each completed turn. Opt out with `autosave = false` and call
    /// `dialog:save()` manually.
    pub autosave: bool,

    /// RFC3339 timestamp of the original creation, preserved across resumes.
    pub created_at: String,
}

impl AgentDialog {
    /// Build a fresh dialog. Emits a `dialog_started` event.
    /// Caller must ensure `agents.len() >= 2` and `starting_speaker < agents.len()`.
    /// Build a fresh (or resumed) dialog.
    ///
    /// When `store` and `id` are both provided, the store is consulted for
    /// a prior `DialogStateRecord` with that id. On hit, the persisted
    /// transcript, `next_index`, and stop state are reinstated so the
    /// dialog picks up where it left off. On miss (or when no `id` was
    /// supplied) the dialog starts fresh.
    #[allow(clippy::too_many_arguments)]
    pub async fn new_or_resume(
        agents: Vec<Agent>,
        provider: Arc<dyn LlmProvider>,
        tool_registry: ToolRegistry,
        model: String,
        starter: String,
        max_turns: usize,
        max_history: Option<usize>,
        stream: bool,
        max_tool_rounds: usize,
        starting_speaker: usize,
        eventbus: EventBus,
        turn_selector_key: Option<mlua::RegistryKey>,
        should_stop_key: Option<mlua::RegistryKey>,
        id: Option<String>,
        store: Option<Arc<dyn StateStore>>,
        flow_name: String,
        autosave: bool,
    ) -> Result<Self, IronCrewError> {
        let now = chrono::Utc::now().to_rfc3339();

        let (id, persistent) = match id {
            Some(s) => {
                validate_session_id(&s)?;
                (s, true)
            }
            None => (uuid::Uuid::new_v4().to_string(), false),
        };

        // Fresh defaults; overridden by a successful resume.
        let mut transcript = VecDeque::new();
        let mut next_index = 0usize;
        let mut stopped = false;
        let mut stop_reason: Option<String> = None;
        let mut created_at = now.clone();

        if persistent
            && let Some(ref store) = store
            && let Some(record) = store.get_dialog_state(&id).await?
        {
            // Sanity-check the resumed record — the agent list should
            // match what the caller is setting up now. If it doesn't, we
            // fail loud rather than silently resume with mismatched state.
            let current_names: Vec<String> = agents.iter().map(|a| a.name.clone()).collect();
            if record.agent_names != current_names {
                return Err(IronCrewError::Validation(format!(
                    "Dialog '{}' was saved with agents {:?} but is being resumed with {:?}",
                    id, record.agent_names, current_names
                )));
            }
            transcript = record.transcript.into();
            next_index = record.next_index;
            stopped = record.stopped;
            stop_reason = record.stop_reason;
            created_at = record.created_at;
            tracing::info!(
                "Resumed dialog '{}' at turn {} ({} prior turns)",
                id,
                next_index,
                transcript.len()
            );
        }

        eventbus.emit(CrewEvent::DialogStarted {
            dialog_id: id.clone(),
            agents: agents.iter().map(|a| a.name.clone()).collect(),
            max_turns,
        });

        Ok(Self {
            id,
            agents,
            provider,
            tool_registry,
            model,
            starter,
            max_turns,
            max_history,
            stream,
            max_tool_rounds,
            starting_speaker,
            transcript: Mutex::new(transcript),
            next_index: Mutex::new(next_index),
            eventbus,
            completed_emitted: Mutex::new(false),
            turn_selector_key,
            should_stop_key,
            stopped: Mutex::new(stopped),
            stop_reason: Mutex::new(stop_reason),
            persistent,
            store: if persistent { store } else { None },
            flow_name,
            autosave,
            created_at,
        })
    }

    /// Persist the current dialog state to the configured store.
    /// No-ops for non-persistent sessions.
    pub async fn persist(&self) -> Result<(), IronCrewError> {
        let Some(ref store) = self.store else {
            return Ok(());
        };
        if !self.persistent {
            return Ok(());
        }
        let transcript: Vec<DialogTurn> = self.transcript.lock().await.iter().cloned().collect();
        let next_index = *self.next_index.lock().await;
        let stopped = *self.stopped.lock().await;
        let stop_reason = self.stop_reason.lock().await.clone();
        let record = DialogStateRecord {
            id: self.id.clone(),
            flow_name: self.flow_name.clone(),
            agent_names: self.agents.iter().map(|a| a.name.clone()).collect(),
            starter: self.starter.clone(),
            transcript,
            next_index,
            stopped,
            stop_reason,
            created_at: self.created_at.clone(),
            updated_at: chrono::Utc::now().to_rfc3339(),
        };
        store.save_dialog_state(&record).await
    }

    /// Returns `true` if the dialog has not reached `max_turns` yet AND no
    /// `should_stop` callback has requested termination.
    async fn has_turns_remaining(&self) -> bool {
        if *self.stopped.lock().await {
            return false;
        }
        *self.next_index.lock().await < self.max_turns
    }

    /// Interpret a Lua return value from the `should_stop` callback.
    ///
    /// Accepted shapes:
    ///   - `nil` / `false` → continue (returns `None`)
    ///   - `true`          → stop with the default reason `"custom_stop"`
    ///   - `"reason"`      → stop with the given reason
    ///
    /// Anything else (numbers, tables, etc.) is a usage error and surfaces as
    /// a validation error so the user gets a clear message instead of a
    /// silent no-op.
    fn interpret_stop_value(value: mlua::Value) -> Result<Option<String>, IronCrewError> {
        match value {
            mlua::Value::Nil => Ok(None),
            mlua::Value::Boolean(false) => Ok(None),
            mlua::Value::Boolean(true) => Ok(Some("custom_stop".into())),
            mlua::Value::String(s) => {
                let owned = s
                    .to_str()
                    .map(|s| s.to_string())
                    .map_err(|e| IronCrewError::Validation(format!("should_stop reason: {}", e)))?;
                if owned.is_empty() {
                    Ok(Some("custom_stop".into()))
                } else {
                    Ok(Some(owned))
                }
            }
            other => Err(IronCrewError::Validation(format!(
                "should_stop callback must return nil, bool, or string — got {}",
                other.type_name()
            ))),
        }
    }

    /// After a turn has been executed, consult the `should_stop` callback (if
    /// any). If it signals stop, record the reason, flip the `stopped` flag,
    /// and emit `DialogCompleted` (guarded by `completed_emitted` so the
    /// max-turns path can't double-emit).
    async fn maybe_stop_after_turn(
        &self,
        lua: &mlua::Lua,
        turn: &DialogTurn,
    ) -> Result<(), IronCrewError> {
        let Some(ref key) = self.should_stop_key else {
            return Ok(());
        };

        let func: mlua::Function = lua
            .registry_value(key)
            .map_err(|e| IronCrewError::Validation(format!("should_stop callback: {}", e)))?;

        // Build last-turn table for the callback
        let last_turn_table = turn_to_lua(lua, turn)
            .map_err(|e| IronCrewError::Validation(format!("should_stop: {}", e)))?;

        // Build transcript snapshot for the callback
        let transcript = self.transcript.lock().await;
        let transcript_table: Vec<DialogTurn> = transcript.iter().cloned().collect();
        drop(transcript);
        let transcript_lua = transcript_to_lua(lua, &transcript_table)
            .map_err(|e| IronCrewError::Validation(format!("should_stop: {}", e)))?;

        // Call via call_async so users can use async methods (e.g. a
        // moderator:send() check) inside the callback.
        let result: mlua::Value = func
            .call_async((last_turn_table, transcript_lua))
            .await
            .map_err(|e| IronCrewError::Validation(format!("should_stop callback: {}", e)))?;

        let Some(reason) = Self::interpret_stop_value(result)? else {
            return Ok(());
        };

        *self.stopped.lock().await = true;
        *self.stop_reason.lock().await = Some(reason.clone());

        let mut emitted = self.completed_emitted.lock().await;
        if !*emitted {
            *emitted = true;
            let total = *self.next_index.lock().await;
            self.eventbus.emit(CrewEvent::DialogCompleted {
                dialog_id: self.id.clone(),
                total_turns: total,
                stop_reason: Some(reason),
            });
        }
        Ok(())
    }

    /// Default round-robin speaker selection.
    async fn round_robin_speaker(&self) -> usize {
        let next_idx = *self.next_index.lock().await;
        (self.starting_speaker + next_idx) % self.agents.len()
    }

    /// Resolve the next speaker — uses turn_selector callback if present,
    /// otherwise falls back to round-robin.
    async fn select_speaker(&self, lua: &mlua::Lua) -> Result<usize, IronCrewError> {
        if let Some(ref key) = self.turn_selector_key {
            let func: mlua::Function = lua
                .registry_value(key)
                .map_err(|e| IronCrewError::Validation(format!("turn_selector callback: {}", e)))?;

            // Build transcript table for the callback
            let transcript = self.transcript.lock().await;
            let transcript_table = lua
                .create_table()
                .map_err(|e| IronCrewError::Validation(e.to_string()))?;
            for (i, turn) in transcript.iter().enumerate() {
                let entry = lua
                    .create_table()
                    .map_err(|e| IronCrewError::Validation(e.to_string()))?;
                entry
                    .set("index", turn.index)
                    .map_err(|e| IronCrewError::Validation(e.to_string()))?;
                entry
                    .set("speaker", speaker_label(turn.speaker_index))
                    .map_err(|e| IronCrewError::Validation(e.to_string()))?;
                entry
                    .set("agent", turn.agent_name.clone())
                    .map_err(|e| IronCrewError::Validation(e.to_string()))?;
                entry
                    .set("content", turn.content.clone())
                    .map_err(|e| IronCrewError::Validation(e.to_string()))?;
                transcript_table
                    .set(i + 1, entry)
                    .map_err(|e| IronCrewError::Validation(e.to_string()))?;
            }
            drop(transcript);

            // Build agents name table
            let agents_table = lua
                .create_table()
                .map_err(|e| IronCrewError::Validation(e.to_string()))?;
            for (i, a) in self.agents.iter().enumerate() {
                agents_table
                    .set(i + 1, a.name.clone())
                    .map_err(|e| IronCrewError::Validation(e.to_string()))?;
            }

            // Call the callback (supports async methods like moderator:send())
            let result: String = func
                .call_async((transcript_table, agents_table))
                .await
                .map_err(|e| {
                    IronCrewError::Validation(format!("turn_selector returned error: {}", e))
                })?;

            // Resolve agent name to index
            let name = result.trim();
            self.agents
                .iter()
                .position(|a| a.name == name)
                .ok_or_else(|| {
                    IronCrewError::Validation(format!(
                        "turn_selector returned unknown agent '{}'. Valid: [{}]",
                        name,
                        self.agents
                            .iter()
                            .map(|a| a.name.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ))
                })
        } else {
            Ok(self.round_robin_speaker().await)
        }
    }

    /// Resolve an agent name to its index in this dialog.
    fn agent_index(&self, name: &str) -> Result<usize, IronCrewError> {
        self.agents
            .iter()
            .position(|a| a.name == name)
            .ok_or_else(|| {
                IronCrewError::Validation(format!(
                    "Agent '{}' not in this dialog. Participants: [{}]",
                    name,
                    self.agents
                        .iter()
                        .map(|a| a.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ))
            })
    }

    /// Build the message list from the perspective of the agent at `speaker_index`.
    /// - System: that agent's system prompt
    /// - Starter as user
    /// - Their own past turns as assistant, others' as user with `[name]:` prefix
    async fn build_messages(&self, speaker_index: usize) -> Vec<ChatMessage> {
        let agent = &self.agents[speaker_index];

        let system_content = agent
            .system_prompt
            .clone()
            .unwrap_or_else(|| format!("You are {}. Your goal: {}", agent.name, agent.goal));

        let mut messages = vec![ChatMessage::system(&system_content)];

        // The starter is always a user message
        messages.push(ChatMessage::user(&self.starter));

        // Walk transcript and assign roles based on perspective
        let transcript = self.transcript.lock().await;
        for turn in transcript.iter() {
            if turn.speaker_index == speaker_index {
                messages.push(ChatMessage::assistant(Some(turn.content.clone()), None));
            } else {
                let prefixed = format!("[{}]: {}", turn.agent_name, turn.content);
                messages.push(ChatMessage::user(&prefixed));
            }
        }

        // Apply history cap (keep system + starter + last N turns)
        if let Some(cap) = self.max_history {
            // System (1) + starter (1) = 2 always preserved at start
            let limit = cap + 2;
            if messages.len() > limit {
                let excess = messages.len() - limit;
                messages.drain(2..2 + excess);
            }
        }

        messages
    }

    /// Run a single turn with automatic speaker selection (round-robin or callback).
    /// Returns `None` if the dialog has already reached max_turns or was
    /// stopped by a `should_stop` callback on a previous turn.
    pub async fn run_one_turn(&self, lua: &mlua::Lua) -> Result<Option<DialogTurn>, IronCrewError> {
        if !self.has_turns_remaining().await {
            return Ok(None);
        }
        let speaker_index = self.select_speaker(lua).await?;
        let turn = self.execute_turn(speaker_index).await?;
        self.maybe_stop_after_turn(lua, &turn).await?;
        self.autosave_if_enabled().await;
        Ok(Some(turn))
    }

    /// Run a turn for a specific agent by name. Useful for moderator-driven
    /// loops where the caller picks who speaks next.
    /// Returns `None` if max_turns is reached or a prior turn triggered stop.
    pub async fn run_turn_for(
        &self,
        lua: &mlua::Lua,
        agent_name: &str,
    ) -> Result<Option<DialogTurn>, IronCrewError> {
        if !self.has_turns_remaining().await {
            return Ok(None);
        }
        let speaker_index = self.agent_index(agent_name)?;
        let turn = self.execute_turn(speaker_index).await?;
        self.maybe_stop_after_turn(lua, &turn).await?;
        self.autosave_if_enabled().await;
        Ok(Some(turn))
    }

    /// Persist the dialog state after a turn, if autosave is enabled and
    /// the session is persistent. Errors are logged but not propagated so
    /// a transient store failure doesn't kill an in-progress dialog.
    async fn autosave_if_enabled(&self) {
        if !self.autosave || !self.persistent {
            return;
        }
        if let Err(e) = self.persist().await {
            tracing::warn!("Autosave failed for dialog '{}': {}", self.id, e);
        }
    }

    /// Execute a turn for the agent at `speaker_index`. Increments the turn
    /// counter and emits SSE events.
    async fn execute_turn(&self, speaker_index: usize) -> Result<DialogTurn, IronCrewError> {
        let agent = self.agents[speaker_index].clone();
        let messages = self.build_messages(speaker_index).await;
        let tool_schemas = self.tool_registry.schemas_for(&agent.tools);
        let has_tools = !tool_schemas.is_empty();

        // For streaming, prefix the output with [agent_name]
        if self.stream {
            eprint!("\x1b[1m[{}]\x1b[0m ", agent.name);
            std::io::Write::flush(&mut std::io::stderr()).ok();
        }

        // Tool-call loop (mirrors LuaConversation::run_turn)
        let mut accumulated_reasoning = String::new();
        let accumulated_content;
        let mut working_messages = messages;
        let mut rounds = 0usize;

        loop {
            let request = ChatRequest {
                messages: working_messages.clone(),
                model: self.model.clone(),
                temperature: agent.temperature,
                max_tokens: agent.max_tokens,
                response_format: agent.response_format.clone(),
                prompt_cache_key: None,
                prompt_cache_retention: None,
            };

            let response: ChatResponse = if self.stream && !has_tools {
                self.call_streaming(request).await?
            } else if has_tools {
                self.provider
                    .chat_with_tools(request, &tool_schemas)
                    .await?
            } else {
                self.provider.chat(request).await?
            };

            if let Some(ref r) = response.reasoning {
                if !accumulated_reasoning.is_empty() {
                    accumulated_reasoning.push('\n');
                }
                accumulated_reasoning.push_str(r);
            }

            if response.tool_calls.is_empty() {
                let content = response
                    .content
                    .ok_or_else(|| IronCrewError::Provider("Empty response from LLM".into()))?;
                accumulated_content = content;
                break;
            }

            rounds += 1;
            if rounds > self.max_tool_rounds {
                return Err(IronCrewError::Validation(format!(
                    "Dialog turn exceeded max tool rounds ({}) for agent '{}'",
                    self.max_tool_rounds, agent.name
                )));
            }

            // Append assistant tool-call request to working messages
            working_messages.push(ChatMessage::assistant(
                response.content.clone(),
                Some(response.tool_calls.clone()),
            ));

            for tool_call in &response.tool_calls {
                let result_text = self.execute_tool_call(tool_call).await;
                working_messages.push(ChatMessage::tool(&tool_call.id, &result_text));
            }
        }

        let next_index = {
            let mut idx = self.next_index.lock().await;
            let current = *idx;
            *idx = current + 1;
            current
        };

        let turn = DialogTurn {
            index: next_index,
            speaker_index,
            agent_name: agent.name.clone(),
            content: accumulated_content,
            reasoning: if accumulated_reasoning.is_empty() {
                None
            } else {
                Some(accumulated_reasoning)
            },
        };

        {
            let mut transcript = self.transcript.lock().await;
            transcript.push_back(turn.clone());
            // Trim the stored transcript if a cap is configured. This keeps
            // the stored transcript bounded in the same way `build_messages`
            // already bounds the ephemeral prompt message list.
            if let Some(cap) = self.max_history
                && cap > 0
            {
                while transcript.len() > cap {
                    transcript.pop_front();
                }
            }
        }

        // Emit SSE events for this turn
        let speaker_str = speaker_label(speaker_index);
        self.eventbus.emit(CrewEvent::DialogTurn {
            dialog_id: self.id.clone(),
            turn_index: turn.index,
            speaker: speaker_str.clone(),
            agent: turn.agent_name.clone(),
            content: turn.content.clone(),
        });

        if let Some(ref r) = turn.reasoning {
            self.eventbus.emit(CrewEvent::DialogThinking {
                dialog_id: self.id.clone(),
                turn_index: turn.index,
                speaker: speaker_str.clone(),
                agent: turn.agent_name.clone(),
                content: r.clone(),
            });
        }

        if self.stream {
            eprintln!();
            eprintln!();
        }

        // If this turn was the last one, emit dialog_completed for the
        // natural max-turns path. Early `should_stop` termination is handled
        // in `maybe_stop_after_turn`, and `completed_emitted` guarantees only
        // one of the two paths actually fires the event.
        if next_index + 1 >= self.max_turns {
            let mut emitted = self.completed_emitted.lock().await;
            if !*emitted {
                *emitted = true;
                self.eventbus.emit(CrewEvent::DialogCompleted {
                    dialog_id: self.id.clone(),
                    total_turns: next_index + 1,
                    stop_reason: None,
                });
            }
        }

        Ok(turn)
    }

    async fn call_streaming(&self, request: ChatRequest) -> Result<ChatResponse, IronCrewError> {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<StreamChunk>(100);

        let print_handle = tokio::spawn(async move {
            use std::io::Write;
            while let Some(chunk) = rx.recv().await {
                match chunk {
                    StreamChunk::Text(text) => {
                        eprint!("{}", text);
                        std::io::stderr().flush().ok();
                    }
                    StreamChunk::Thinking(text) => {
                        eprint!("\x1b[90m{}\x1b[0m", text);
                        std::io::stderr().flush().ok();
                    }
                    StreamChunk::Done => {}
                    StreamChunk::Error(e) => {
                        eprintln!("\n[Stream error: {}]", e);
                    }
                    _ => {}
                }
            }
        });

        let result = self.provider.chat_stream(request, tx).await;
        print_handle.await.ok();
        result
    }

    async fn execute_tool_call(&self, tool_call: &ToolCallRequest) -> String {
        let args: serde_json::Value = serde_json::from_str(&tool_call.function.arguments)
            .unwrap_or(serde_json::Value::Object(serde_json::Map::new()));

        let tool_timeout = Duration::from_secs(
            std::env::var("IRONCREW_TOOL_TIMEOUT")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(60),
        );

        let tool_result = match tokio::time::timeout(
            tool_timeout,
            self.tool_registry.execute(&tool_call.function.name, args),
        )
        .await
        {
            Ok(result) => result,
            Err(_) => Err(IronCrewError::ToolExecution {
                tool: tool_call.function.name.clone(),
                message: format!("Tool timed out after {}s", tool_timeout.as_secs()),
            }),
        };

        match tool_result {
            Ok(output) => output,
            Err(e) => format!("Tool error: {}", e),
        }
    }

    /// Run all remaining turns sequentially.
    pub async fn run_all(&self, lua: &mlua::Lua) -> Result<Vec<DialogTurn>, IronCrewError> {
        loop {
            let turn = self.run_one_turn(lua).await?;
            if turn.is_none() {
                break;
            }
        }
        Ok(self.transcript.lock().await.iter().cloned().collect())
    }
}

impl UserData for AgentDialog {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        // dialog:run() — run all turns and return the full transcript
        methods.add_async_method("run", |lua, this, ()| async move {
            let transcript = this.run_all(&lua).await.map_err(mlua::Error::external)?;
            transcript_to_lua(&lua, &transcript)
        });

        // dialog:next_turn() — run a single turn (round-robin or callback), returns the turn or nil
        methods.add_async_method("next_turn", |lua, this, ()| async move {
            let turn = this
                .run_one_turn(&lua)
                .await
                .map_err(mlua::Error::external)?;
            match turn {
                Some(t) => Ok(Value::Table(turn_to_lua(&lua, &t)?)),
                None => Ok(Value::Nil),
            }
        });

        // dialog:next_turn_from(agent_name) — force a specific agent to speak next
        methods.add_async_method(
            "next_turn_from",
            |lua, this, agent_name: String| async move {
                let turn = this
                    .run_turn_for(&lua, &agent_name)
                    .await
                    .map_err(mlua::Error::external)?;
                match turn {
                    Some(t) => Ok(Value::Table(turn_to_lua(&lua, &t)?)),
                    None => Ok(Value::Nil),
                }
            },
        );

        // dialog:transcript() — get the current transcript
        methods.add_async_method("transcript", |lua, this, ()| async move {
            let transcript: Vec<DialogTurn> =
                this.transcript.lock().await.iter().cloned().collect();
            transcript_to_lua(&lua, &transcript)
        });

        // dialog:turn_count() — number of completed turns
        methods.add_async_method("turn_count", |_, this, ()| async move {
            Ok(*this.next_index.lock().await)
        });

        // dialog:current_speaker() — positional letter ("a", "b", ...) or nil if finished
        methods.add_async_method("current_speaker", |_, this, ()| async move {
            if this.has_turns_remaining().await {
                let idx = this.round_robin_speaker().await;
                Ok(Some(speaker_label(idx)))
            } else {
                Ok(None)
            }
        });

        // dialog:current_agent() — name of the next agent to speak (round-robin), or nil
        methods.add_async_method("current_agent", |_, this, ()| async move {
            if this.has_turns_remaining().await {
                let idx = this.round_robin_speaker().await;
                Ok(Some(this.agents[idx].name.clone()))
            } else {
                Ok(None)
            }
        });

        // dialog:agents() — list of agent names participating in this dialog
        methods.add_method("agents", |lua, this, ()| {
            let table = lua.create_table()?;
            for (i, a) in this.agents.iter().enumerate() {
                table.set(i + 1, a.name.clone())?;
            }
            Ok(table)
        });

        // dialog:reset() — clear transcript and reset to turn 0
        methods.add_async_method("reset", |_, this, ()| async move {
            *this.next_index.lock().await = 0;
            this.transcript.lock().await.clear();
            Ok(())
        });

        // dialog:max_turns() — configured turn limit
        methods.add_method("max_turns", |_, this, ()| Ok(this.max_turns));

        // dialog:stopped() — true if a should_stop callback has requested termination
        methods.add_async_method("stopped", |_, this, ()| async move {
            Ok(*this.stopped.lock().await)
        });

        // dialog:stop_reason() — the reason string if stopped early, otherwise nil
        methods.add_async_method("stop_reason", |_, this, ()| async move {
            Ok(this.stop_reason.lock().await.clone())
        });

        // dialog:id() — the stable session id (user-provided or auto-UUID)
        methods.add_method("id", |_, this, ()| Ok(this.id.clone()));

        // dialog:is_persistent() — true if tied to a store for cross-run resume
        methods.add_method("is_persistent", |_, this, ()| Ok(this.persistent));

        // dialog:save() — explicit save (useful when autosave = false)
        methods.add_async_method("save", |_, this, ()| async move {
            this.persist().await.map_err(mlua::Error::external)
        });

        // dialog:delete() — remove the persisted record
        methods.add_async_method("delete", |_, this, ()| async move {
            if let Some(ref store) = this.store {
                store
                    .delete_dialog_state(&this.id)
                    .await
                    .map_err(mlua::Error::external)?;
            }
            Ok(())
        });
    }
}

fn turn_to_lua(lua: &mlua::Lua, turn: &DialogTurn) -> mlua::Result<Table> {
    let table = lua.create_table()?;
    table.set("index", turn.index)?;
    table.set("speaker", speaker_label(turn.speaker_index))?;
    table.set("agent", turn.agent_name.clone())?;
    table.set("content", turn.content.clone())?;
    if let Some(ref r) = turn.reasoning {
        table.set("reasoning", r.clone())?;
    }
    Ok(table)
}

fn transcript_to_lua(lua: &mlua::Lua, transcript: &[DialogTurn]) -> mlua::Result<Table> {
    let table = lua.create_table()?;
    for (i, turn) in transcript.iter().enumerate() {
        table.set(i + 1, turn_to_lua(lua, turn)?)?;
    }
    Ok(table)
}

/// Build an AgentDialog from a Lua options table. Participants are given via
/// the `agents = {"name", ...}` array (two or more).
///
/// When `store` and the caller-provided `id` are both present, the dialog is
/// resumed from the store if a prior record exists (transcript, `next_index`,
/// and stop state). Autosave defaults to `true` for persistent sessions.
#[allow(clippy::too_many_arguments)]
pub async fn build_dialog(
    lua: &mlua::Lua,
    table: Table,
    crew_agents: &[Agent],
    provider: Arc<dyn LlmProvider>,
    tool_registry: ToolRegistry,
    crew_default_model: &str,
    crew_max_tool_rounds: usize,
    eventbus: EventBus,
    store: Option<Arc<dyn StateStore>>,
    flow_name: String,
) -> mlua::Result<AgentDialog> {
    let agents_table = table.get::<Table>("agents").map_err(|_| {
        mlua::Error::external(IronCrewError::Validation(
            "Dialog requires an `agents = {\"name\", ...}` array of two or more \
             participants"
                .into(),
        ))
    })?;
    let mut dialog_agents: Vec<Agent> = Vec::new();
    for value in agents_table.sequence_values::<Value>() {
        let value = value?;
        dialog_agents.push(resolve_agent(value, crew_agents, "agents")?);
    }

    if dialog_agents.len() < 2 {
        return Err(mlua::Error::external(IronCrewError::Validation(
            "Dialog requires at least 2 agents".into(),
        )));
    }

    // Reject duplicate names — each agent must be distinct
    {
        let mut seen = std::collections::HashSet::new();
        for a in &dialog_agents {
            if !seen.insert(a.name.as_str()) {
                return Err(mlua::Error::external(IronCrewError::Validation(format!(
                    "Dialog: agent '{}' is listed more than once",
                    a.name
                ))));
            }
        }
    }

    let starter: String = table.get("starter").map_err(|_| {
        mlua::Error::external(IronCrewError::Validation(
            "Dialog requires a 'starter' string".into(),
        ))
    })?;

    let max_turns: usize = table
        .get::<usize>("max_turns")
        .unwrap_or(dialog_agents.len() * 2);
    // max_history resolution order (same pattern as LuaConversation):
    //   1. Explicit value in the Lua table (0 → unbounded opt-in)
    //   2. IRONCREW_DIALOG_MAX_HISTORY env var
    //   3. Safe default of 100 turns
    let max_history: Option<usize> = match table.get::<usize>("max_history") {
        Ok(0) => None,
        Ok(n) => Some(n),
        Err(_) => {
            let env_default = std::env::var("IRONCREW_DIALOG_MAX_HISTORY")
                .ok()
                .and_then(|v| v.parse::<usize>().ok());
            match env_default {
                Some(0) => None,
                Some(n) => Some(n),
                None => Some(100),
            }
        }
    };
    let stream: bool = table.get::<bool>("stream").unwrap_or(false);

    // starting_speaker accepts:
    //   - an agent name (preferred for multi-party)
    //   - a positional letter "a", "b", "c", ...
    //   - default: first agent (index 0)
    let starting_speaker: usize = match table.get::<String>("starting_speaker").ok() {
        Some(s) => {
            // Try as agent name first
            if let Some(idx) = dialog_agents.iter().position(|a| a.name == s) {
                idx
            } else if s.len() == 1 {
                let c = s.chars().next().unwrap().to_ascii_lowercase();
                if c.is_ascii_alphabetic() {
                    let idx = (c as u8 - b'a') as usize;
                    if idx < dialog_agents.len() {
                        idx
                    } else {
                        return Err(mlua::Error::external(IronCrewError::Validation(format!(
                            "Dialog: starting_speaker '{}' is out of range (only {} agents)",
                            s,
                            dialog_agents.len()
                        ))));
                    }
                } else {
                    0
                }
            } else {
                return Err(mlua::Error::external(IronCrewError::Validation(format!(
                    "Dialog: starting_speaker '{}' does not match any agent in this dialog",
                    s
                ))));
            }
        }
        None => 0,
    };

    let model: String = table
        .get::<String>("model")
        .ok()
        .unwrap_or_else(|| crew_default_model.to_string());

    // Optional turn_selector callback — stored in the Lua registry for thread safety
    let turn_selector_key: Option<mlua::RegistryKey> =
        if let Ok(func) = table.get::<mlua::Function>("turn_selector") {
            Some(lua.create_registry_value(func)?)
        } else {
            None
        };

    // Optional should_stop callback — same registry-key pattern as turn_selector
    let should_stop_key: Option<mlua::RegistryKey> =
        if let Ok(func) = table.get::<mlua::Function>("should_stop") {
            Some(lua.create_registry_value(func)?)
        } else {
            None
        };

    // Cross-run persistence: `id` is the persistence key. When omitted the
    // dialog is ephemeral (pre-2.8 behavior — a fresh UUID is generated).
    let id: Option<String> = table.get::<String>("id").ok();
    let autosave: bool = table.get::<bool>("autosave").unwrap_or(true);

    AgentDialog::new_or_resume(
        dialog_agents,
        provider,
        tool_registry,
        model,
        starter,
        max_turns,
        max_history,
        stream,
        crew_max_tool_rounds,
        starting_speaker,
        eventbus,
        turn_selector_key,
        should_stop_key,
        id,
        store,
        flow_name,
        autosave,
    )
    .await
    .map_err(mlua::Error::external)
}

fn resolve_agent(value: Value, agents: &[Agent], field: &str) -> mlua::Result<Agent> {
    match value {
        Value::String(s) => {
            let name = s.to_str()?.to_string();
            agents
                .iter()
                .find(|a| a.name == name)
                .cloned()
                .ok_or_else(|| {
                    mlua::Error::external(IronCrewError::Validation(format!(
                        "Dialog: {} agent '{}' not found in crew",
                        field, name
                    )))
                })
        }
        Value::Table(t) => crate::lua::parsers::agent_from_lua_table(&t),
        _ => Err(mlua::Error::external(IronCrewError::Validation(format!(
            "Dialog: {} must be a string (agent name) or Agent table",
            field
        )))),
    }
}

#[cfg(test)]
mod interpret_stop_tests {
    use super::*;

    fn lua() -> mlua::Lua {
        mlua::Lua::new()
    }

    #[test]
    fn nil_means_continue() {
        let result = AgentDialog::interpret_stop_value(mlua::Value::Nil).unwrap();
        assert_eq!(result, None);
    }

    #[test]
    fn false_means_continue() {
        let result = AgentDialog::interpret_stop_value(mlua::Value::Boolean(false)).unwrap();
        assert_eq!(result, None);
    }

    #[test]
    fn true_means_stop_with_default_reason() {
        let result = AgentDialog::interpret_stop_value(mlua::Value::Boolean(true)).unwrap();
        assert_eq!(result.as_deref(), Some("custom_stop"));
    }

    #[test]
    fn string_means_stop_with_that_reason() {
        let lua = lua();
        let s = lua.create_string("consensus reached").unwrap();
        let result = AgentDialog::interpret_stop_value(mlua::Value::String(s)).unwrap();
        assert_eq!(result.as_deref(), Some("consensus reached"));
    }

    #[test]
    fn empty_string_falls_back_to_default_reason() {
        let lua = lua();
        let s = lua.create_string("").unwrap();
        let result = AgentDialog::interpret_stop_value(mlua::Value::String(s)).unwrap();
        assert_eq!(result.as_deref(), Some("custom_stop"));
    }

    #[test]
    fn number_is_rejected_as_usage_error() {
        let result = AgentDialog::interpret_stop_value(mlua::Value::Integer(42));
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("must return nil, bool, or string"),
            "unexpected error: {}",
            err
        );
    }

    #[test]
    fn table_is_rejected_as_usage_error() {
        let lua = lua();
        let t = lua.create_table().unwrap();
        let result = AgentDialog::interpret_stop_value(mlua::Value::Table(t));
        assert!(result.is_err());
    }
}
