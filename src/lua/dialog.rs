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

/// Which agent slot is the active speaker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DialogSpeaker {
    A,
    B,
}

impl DialogSpeaker {
    fn other(self) -> Self {
        match self {
            DialogSpeaker::A => DialogSpeaker::B,
            DialogSpeaker::B => DialogSpeaker::A,
        }
    }

    fn label(self) -> &'static str {
        match self {
            DialogSpeaker::A => "a",
            DialogSpeaker::B => "b",
        }
    }
}

/// One turn in the dialog transcript.
#[derive(Debug, Clone)]
pub struct DialogTurn {
    pub index: usize,
    pub speaker: DialogSpeaker,
    pub agent_name: String,
    pub content: String,
    pub reasoning: Option<String>,
}

/// State of an agent-to-agent dialog.
pub struct AgentDialog {
    /// Stable identifier — included in every SSE event for this dialog.
    pub id: String,

    pub agent_a: Agent,
    pub agent_b: Agent,

    pub provider: Arc<dyn LlmProvider>,
    pub tool_registry: ToolRegistry,
    pub model: String,

    /// The kickoff message that the first speaker responds to.
    pub starter: String,

    pub max_turns: usize,
    pub max_history: Option<usize>,
    pub stream: bool,
    pub max_tool_rounds: usize,
    pub starting_speaker: DialogSpeaker,

    /// The shared transcript — turns in chronological order.
    pub transcript: Mutex<Vec<DialogTurn>>,
    /// Index of the next turn to run (0-based).
    pub next_index: Mutex<usize>,

    /// EventBus for emitting dialog_* SSE events.
    pub eventbus: EventBus,

    /// Tracks whether dialog_completed has been emitted (set to true after run_all
    /// reaches max_turns) so it isn't emitted twice for the same dialog.
    pub completed_emitted: Mutex<bool>,
}

impl AgentDialog {
    /// Build a fresh dialog. Emits a `dialog_started` event.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        agent_a: Agent,
        agent_b: Agent,
        provider: Arc<dyn LlmProvider>,
        tool_registry: ToolRegistry,
        model: String,
        starter: String,
        max_turns: usize,
        max_history: Option<usize>,
        stream: bool,
        max_tool_rounds: usize,
        starting_speaker: DialogSpeaker,
        eventbus: EventBus,
    ) -> Self {
        let id = uuid::Uuid::new_v4().to_string();

        eventbus.emit(CrewEvent::DialogStarted {
            dialog_id: id.clone(),
            agent_a: agent_a.name.clone(),
            agent_b: agent_b.name.clone(),
            max_turns,
        });

        Self {
            id,
            agent_a,
            agent_b,
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
        }
    }

    /// Returns the agent that should speak next, or None if the dialog is over.
    async fn next_speaker(&self) -> Option<DialogSpeaker> {
        let next_idx = *self.next_index.lock().await;
        if next_idx >= self.max_turns {
            return None;
        }
        // Even indices = starting_speaker, odd indices = the other.
        let speaker = if next_idx % 2 == 0 {
            self.starting_speaker
        } else {
            self.starting_speaker.other()
        };
        Some(speaker)
    }

    fn agent_for(&self, speaker: DialogSpeaker) -> &Agent {
        match speaker {
            DialogSpeaker::A => &self.agent_a,
            DialogSpeaker::B => &self.agent_b,
        }
    }

    /// Build the message list from the perspective of `speaker`.
    /// - System: that speaker's system prompt
    /// - Starter as user
    /// - Their own past turns as assistant, opponent's as user with [name]: prefix
    async fn build_messages(&self, speaker: DialogSpeaker) -> Vec<ChatMessage> {
        let agent = self.agent_for(speaker);
        let opponent = self.agent_for(speaker.other());

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
            if turn.speaker == speaker {
                messages.push(ChatMessage::assistant(Some(turn.content.clone()), None));
            } else {
                let prefixed = format!("[{}]: {}", opponent.name, turn.content);
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

    /// Run a single turn (the next speaker's reply) and append it to the transcript.
    /// Returns `None` if the dialog has already reached max_turns.
    pub async fn run_one_turn(&self) -> Result<Option<DialogTurn>, IronCrewError> {
        let Some(speaker) = self.next_speaker().await else {
            return Ok(None);
        };

        let agent = self.agent_for(speaker).clone();
        let messages = self.build_messages(speaker).await;
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
            speaker,
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
        self.eventbus.emit(CrewEvent::DialogTurn {
            dialog_id: self.id.clone(),
            turn_index: turn.index,
            speaker: turn.speaker.label().to_string(),
            agent: turn.agent_name.clone(),
            content: turn.content.clone(),
        });

        if let Some(ref r) = turn.reasoning {
            self.eventbus.emit(CrewEvent::DialogThinking {
                dialog_id: self.id.clone(),
                turn_index: turn.index,
                speaker: turn.speaker.label().to_string(),
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

        Ok(Some(turn))
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
    pub async fn run_all(&self) -> Result<Vec<DialogTurn>, IronCrewError> {
        loop {
            let turn = self.run_one_turn().await?;
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
            let transcript = this.run_all().await.map_err(mlua::Error::external)?;
            transcript_to_lua(&lua, &transcript)
        });

        // dialog:next_turn() — run a single turn, returns the turn or nil
        methods.add_async_method("next_turn", |lua, this, ()| async move {
            let turn = this.run_one_turn().await.map_err(mlua::Error::external)?;
            match turn {
                Some(t) => Ok(Value::Table(turn_to_lua(&lua, &t)?)),
                None => Ok(Value::Nil),
            }
        });

        // dialog:transcript() — get the current transcript
        methods.add_async_method("transcript", |lua, this, ()| async move {
            let transcript = this.transcript.lock().await.clone();
            transcript_to_lua(&lua, &transcript)
        });

        // dialog:turn_count() — number of completed turns
        methods.add_async_method("turn_count", |_, this, ()| async move {
            Ok(*this.next_index.lock().await)
        });

        // dialog:current_speaker() — "a" or "b" or nil if dialog is finished
        methods.add_async_method("current_speaker", |_, this, ()| async move {
            Ok(this.next_speaker().await.map(|s| s.label().to_string()))
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
    table.set("speaker", turn.speaker.label())?;
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

/// Build an AgentDialog from a Lua options table.
#[allow(clippy::too_many_arguments)]
pub fn build_dialog(
    table: Table,
    agents: &[Agent],
    provider: Arc<dyn LlmProvider>,
    tool_registry: ToolRegistry,
    crew_default_model: &str,
    crew_max_tool_rounds: usize,
    eventbus: EventBus,
) -> mlua::Result<AgentDialog> {
    let agent_a = resolve_agent(table.get::<Value>("agent_a")?, agents, "agent_a")?;
    let agent_b = resolve_agent(table.get::<Value>("agent_b")?, agents, "agent_b")?;

    if agent_a.name == agent_b.name {
        return Err(mlua::Error::external(IronCrewError::Validation(
            "Dialog requires two distinct agents".into(),
        )));
    }

    let starter: String = table.get("starter").map_err(|_| {
        mlua::Error::external(IronCrewError::Validation(
            "Dialog requires a 'starter' string".into(),
        ))
    })?;

    let max_turns: usize = table.get::<usize>("max_turns").unwrap_or(4);
    let max_history: Option<usize> = table.get("max_history").ok();
    let stream: bool = table.get::<bool>("stream").unwrap_or(false);

    let starting_speaker = match table.get::<String>("starting_speaker").ok().as_deref() {
        Some("b") | Some("B") => DialogSpeaker::B,
        _ => DialogSpeaker::A,
    };

    let model: String = table
        .get::<String>("model")
        .ok()
        .unwrap_or_else(|| crew_default_model.to_string());

    Ok(AgentDialog::new(
        agent_a,
        agent_b,
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
