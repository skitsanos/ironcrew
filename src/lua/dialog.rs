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
//! - SSE wiring (`dialog_turn`, `dialog_thinking` events)
//! - Custom termination conditions via Lua callback
//! - More than two agents (round-robin or moderator-driven)
//! - Cross-run persistence

use std::sync::Arc;
use std::time::Duration;

use mlua::{Table, UserData, UserDataMethods, Value};
use tokio::sync::Mutex;

use crate::engine::agent::Agent;
use crate::engine::eventbus::{CrewEvent, EventBus};
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
#[derive(Debug, Clone)]
pub struct DialogTurn {
    pub index: usize,
    pub speaker_index: usize,
    pub agent_name: String,
    pub content: String,
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
    pub transcript: Mutex<Vec<DialogTurn>>,
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
}

impl AgentDialog {
    /// Build a fresh dialog. Emits a `dialog_started` event.
    /// Caller must ensure `agents.len() >= 2` and `starting_speaker < agents.len()`.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
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
    ) -> Self {
        let id = uuid::Uuid::new_v4().to_string();
        let agent_names: Vec<String> = agents.iter().map(|a| a.name.clone()).collect();

        // For backward compat, the dialog_started event still has agent_a/agent_b
        // fields when there are exactly 2 agents. For 3+ agents we use the
        // first two as the canonical pair (older clients still get something
        // useful) and the full list is reflected in dialog_turn events.
        eventbus.emit(CrewEvent::DialogStarted {
            dialog_id: id.clone(),
            agent_a: agent_names[0].clone(),
            agent_b: agent_names.get(1).cloned().unwrap_or_default(),
            max_turns,
        });

        Self {
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
            transcript: Mutex::new(Vec::new()),
            next_index: Mutex::new(0),
            eventbus,
            completed_emitted: Mutex::new(false),
            turn_selector_key,
        }
    }

    /// Returns `true` if the dialog has not reached `max_turns` yet.
    async fn has_turns_remaining(&self) -> bool {
        *self.next_index.lock().await < self.max_turns
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
    /// Returns `None` if the dialog has already reached max_turns.
    pub async fn run_one_turn(&self, lua: &mlua::Lua) -> Result<Option<DialogTurn>, IronCrewError> {
        if !self.has_turns_remaining().await {
            return Ok(None);
        }
        let speaker_index = self.select_speaker(lua).await?;
        self.execute_turn(speaker_index).await.map(Some)
    }

    /// Run a turn for a specific agent by name. Useful for moderator-driven
    /// loops where the caller picks who speaks next.
    /// Returns `None` if max_turns is reached.
    pub async fn run_turn_for(
        &self,
        agent_name: &str,
    ) -> Result<Option<DialogTurn>, IronCrewError> {
        if !self.has_turns_remaining().await {
            return Ok(None);
        }
        let speaker_index = self.agent_index(agent_name)?;
        self.execute_turn(speaker_index).await.map(Some)
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

        self.transcript.lock().await.push(turn.clone());

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

        // If this turn was the last one, emit dialog_completed
        if next_index + 1 >= self.max_turns {
            let mut emitted = self.completed_emitted.lock().await;
            if !*emitted {
                *emitted = true;
                self.eventbus.emit(CrewEvent::DialogCompleted {
                    dialog_id: self.id.clone(),
                    total_turns: next_index + 1,
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
        Ok(self.transcript.lock().await.clone())
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
                    .run_turn_for(&agent_name)
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
            let transcript = this.transcript.lock().await.clone();
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

/// Build an AgentDialog from a Lua options table. Accepts either a new
/// `agents = {"name", ...}` array OR the legacy `agent_a` / `agent_b` form.
#[allow(clippy::too_many_arguments)]
pub fn build_dialog(
    lua: &mlua::Lua,
    table: Table,
    crew_agents: &[Agent],
    provider: Arc<dyn LlmProvider>,
    tool_registry: ToolRegistry,
    crew_default_model: &str,
    crew_max_tool_rounds: usize,
    eventbus: EventBus,
) -> mlua::Result<AgentDialog> {
    // Resolve participants — prefer the new `agents` array if present.
    let dialog_agents: Vec<Agent> = if let Ok(agents_table) = table.get::<Table>("agents") {
        let mut out: Vec<Agent> = Vec::new();
        for value in agents_table.sequence_values::<Value>() {
            let value = value?;
            out.push(resolve_agent(value, crew_agents, "agents")?);
        }
        out
    } else {
        // Legacy two-agent form
        let agent_a = resolve_agent(table.get::<Value>("agent_a")?, crew_agents, "agent_a")?;
        let agent_b = resolve_agent(table.get::<Value>("agent_b")?, crew_agents, "agent_b")?;
        vec![agent_a, agent_b]
    };

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
    let max_history: Option<usize> = table.get("max_history").ok();
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

    Ok(AgentDialog::new(
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
    ))
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
