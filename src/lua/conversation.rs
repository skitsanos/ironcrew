//! LuaConversation — multi-turn chat with an agent.
//!
//! Created via `crew:conversation({...})`. Maintains its own message history
//! across `send()` / `ask()` calls. Supports tool calling via the crew's
//! tool registry, streaming to stderr, reasoning capture, and optional
//! cross-run persistence keyed by a stable `id`.

use std::sync::Arc;
use std::time::Duration;

use mlua::{Table, UserData, UserDataMethods, Value};
use tokio::sync::Mutex;

use crate::engine::agent::Agent;
use crate::engine::eventbus::{CrewEvent, EventBus};
use crate::engine::sessions::{ConversationRecord, validate_session_id};
use crate::engine::store::StateStore;
use crate::llm::provider::{
    ChatMessage, ChatRequest, ChatResponse, LlmProvider, StreamChunk, ToolCallRequest,
};
use crate::tools::registry::ToolRegistry;
use crate::utils::error::IronCrewError;

/// A stateful, multi-turn conversation with a single agent.
pub struct LuaConversation {
    /// Stable identifier — included in every SSE event for this conversation.
    /// If the user provided one via `id = "..."`, it's the persistence key;
    /// otherwise it's an auto-UUID and the session is not persisted.
    pub id: String,

    /// `true` when the caller provided a stable `id` and the session is
    /// eligible for cross-run persistence.
    pub persistent: bool,

    /// The agent driving the conversation.
    pub agent: Agent,

    /// Provider used for all LLM calls in this conversation.
    pub provider: Arc<dyn LlmProvider>,

    /// Tool registry shared with the parent crew.
    pub tool_registry: ToolRegistry,

    /// Resolved model name.
    pub model: String,

    /// Effective system prompt (override or derived from the agent).
    pub system_prompt: String,

    /// Message history including the system prompt at index 0.
    pub messages: Mutex<Vec<ChatMessage>>,

    /// Optional cap on the number of stored messages (excluding system prompt).
    pub max_history: Option<usize>,

    /// Whether to stream responses to stderr.
    pub stream: bool,

    /// Maximum tool-call rounds per send().
    pub max_tool_rounds: usize,

    /// EventBus for emitting conversation_* SSE events.
    pub eventbus: EventBus,

    /// Optional state store for cross-run persistence. `Some` when the
    /// parent crew was able to instantiate its store *and* the caller
    /// provided an `id`.
    pub store: Option<Arc<dyn StateStore>>,

    /// Flow label persisted alongside the session (taken from `crew.goal`).
    pub flow_name: String,

    /// When `true` (default for persistent sessions), the conversation is
    /// auto-saved to the store after every completed turn. Opt out with
    /// `autosave = false` and call `conversation:save()` manually.
    pub autosave: bool,

    /// RFC3339 timestamp of the original creation (loaded from the store on
    /// resume, or set at construction for fresh sessions).
    pub created_at: String,
}

impl LuaConversation {
    /// Build a fresh (or resumed) conversation.
    ///
    /// When `store` is `Some` and `id` is `Some`, the store is consulted for
    /// a prior record with that id. On hit, the persisted history replaces
    /// the freshly-seeded `[system]` bootstrap so the conversation picks up
    /// where it left off. On miss, a new record will be written on the
    /// first autosave.
    #[allow(clippy::too_many_arguments)]
    pub async fn new_or_resume(
        agent: Agent,
        provider: Arc<dyn LlmProvider>,
        tool_registry: ToolRegistry,
        model: String,
        system_prompt: String,
        max_history: Option<usize>,
        stream: bool,
        max_tool_rounds: usize,
        eventbus: EventBus,
        id: Option<String>,
        store: Option<Arc<dyn StateStore>>,
        flow_name: String,
        autosave: bool,
    ) -> Result<Self, IronCrewError> {
        let now = chrono::Utc::now().to_rfc3339();

        // Resolve the id and decide whether the session is persistent.
        let (id, persistent) = match id {
            Some(s) => {
                validate_session_id(&s)?;
                (s, true)
            }
            None => (uuid::Uuid::new_v4().to_string(), false),
        };

        // Seed the message list. If we can hit the store for a resume, use
        // the persisted messages instead of the bootstrap seed.
        let mut messages = vec![ChatMessage::system(&system_prompt)];
        let mut created_at = now.clone();

        if persistent
            && let Some(ref store) = store
            && let Some(record) = store.get_conversation(&id).await?
        {
            messages = record.messages;
            created_at = record.created_at;
            tracing::info!(
                "Resumed conversation '{}' with {} messages",
                id,
                messages.len()
            );
        }

        eventbus.emit(CrewEvent::ConversationStarted {
            conversation_id: id.clone(),
            agent: agent.name.clone(),
        });

        Ok(Self {
            id,
            persistent,
            agent,
            provider,
            tool_registry,
            model,
            system_prompt,
            messages: Mutex::new(messages),
            max_history,
            stream,
            max_tool_rounds,
            eventbus,
            store: if persistent { store } else { None },
            flow_name,
            autosave,
            created_at,
        })
    }

    /// Persist the current state to the configured store. Safe to call even
    /// for non-persistent sessions — it simply no-ops.
    pub async fn persist(&self) -> Result<(), IronCrewError> {
        let Some(ref store) = self.store else {
            return Ok(());
        };
        if !self.persistent {
            return Ok(());
        }
        let messages = self.messages.lock().await.clone();
        let record = ConversationRecord {
            id: self.id.clone(),
            flow_name: self.flow_name.clone(),
            agent_name: self.agent.name.clone(),
            messages,
            created_at: self.created_at.clone(),
            updated_at: chrono::Utc::now().to_rfc3339(),
        };
        store.save_conversation(&record).await
    }

    /// Run a single send/respond round (with tool-call loop) and return the
    /// full ChatResponse plus any reasoning captured across tool rounds.
    async fn run_turn(
        &self,
        user_message: &str,
    ) -> Result<(String, Option<String>), IronCrewError> {
        // Append the user message to history
        {
            let mut history = self.messages.lock().await;
            history.push(ChatMessage::user(user_message));
            self.enforce_history_cap(&mut history);
        }

        let tool_schemas = self.tool_registry.schemas_for(&self.agent.tools);
        let has_tools = !tool_schemas.is_empty();

        let mut accumulated_reasoning = String::new();
        let mut rounds = 0usize;

        loop {
            // Snapshot the current message list for the request
            let messages_snapshot: Vec<ChatMessage> = {
                let history = self.messages.lock().await;
                history.clone()
            };

            let request = ChatRequest {
                messages: messages_snapshot,
                model: self.model.clone(),
                temperature: self.agent.temperature,
                max_tokens: self.agent.max_tokens,
                response_format: self.agent.response_format.clone(),
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

            // No tool calls → final response
            if response.tool_calls.is_empty() {
                let content = response
                    .content
                    .ok_or_else(|| IronCrewError::Provider("Empty response from LLM".into()))?;

                let turn_index = {
                    let mut history = self.messages.lock().await;
                    history.push(ChatMessage::assistant(Some(content.clone()), None));
                    self.enforce_history_cap(&mut history);
                    // Each completed turn adds 2 messages (user + assistant) on top of system.
                    // turn_index is 0-based: count of user messages already in history minus 1.
                    history
                        .iter()
                        .filter(|m| m.role == "user")
                        .count()
                        .saturating_sub(1)
                };

                let reasoning = if accumulated_reasoning.is_empty() {
                    None
                } else {
                    Some(accumulated_reasoning)
                };

                // Emit SSE events for this completed turn
                self.eventbus.emit(CrewEvent::ConversationTurn {
                    conversation_id: self.id.clone(),
                    agent: self.agent.name.clone(),
                    turn_index,
                    user_message: user_message.to_string(),
                    assistant_message: content.clone(),
                });

                if let Some(ref r) = reasoning {
                    self.eventbus.emit(CrewEvent::ConversationThinking {
                        conversation_id: self.id.clone(),
                        agent: self.agent.name.clone(),
                        turn_index,
                        content: r.clone(),
                    });
                }

                // Autosave after each successful turn (no-op for sessions
                // without a store or with autosave disabled).
                if self.autosave
                    && self.persistent
                    && let Err(e) = self.persist().await
                {
                    tracing::warn!("Autosave failed for conversation '{}': {}", self.id, e);
                }

                return Ok((content, reasoning));
            }

            // Tool round
            rounds += 1;
            if rounds > self.max_tool_rounds {
                return Err(IronCrewError::Validation(format!(
                    "Conversation exceeded max tool rounds ({}) for agent '{}'",
                    self.max_tool_rounds, self.agent.name
                )));
            }

            // Append the assistant's tool-call request to history
            {
                let mut history = self.messages.lock().await;
                history.push(ChatMessage::assistant(
                    response.content.clone(),
                    Some(response.tool_calls.clone()),
                ));
                self.enforce_history_cap(&mut history);
            }

            // Execute each tool call and append results
            for tool_call in &response.tool_calls {
                let result_text = self.execute_tool_call(tool_call).await;
                let mut history = self.messages.lock().await;
                history.push(ChatMessage::tool(&tool_call.id, &result_text));
                self.enforce_history_cap(&mut history);
            }
        }
    }

    /// Stream a request to stderr (with dim reasoning) and return the response.
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
                    StreamChunk::Done => {
                        eprintln!();
                    }
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

    /// Execute a single tool call with the configured timeout, returning a
    /// human-readable result string (errors are stringified into the message).
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

    /// Trim history if it exceeds the configured cap. Always preserves the
    /// system message at index 0.
    fn enforce_history_cap(&self, history: &mut Vec<ChatMessage>) {
        let Some(cap) = self.max_history else {
            return;
        };
        // +1 for the system message we always keep
        let limit = cap + 1;
        if history.len() <= limit {
            return;
        }
        // Drain the oldest non-system messages
        let excess = history.len() - limit;
        history.drain(1..1 + excess);
    }
}

impl UserData for LuaConversation {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        // conv:send(message) → returns plain text
        methods.add_async_method("send", |_, this, message: String| async move {
            let (content, _reasoning) = this
                .run_turn(&message)
                .await
                .map_err(mlua::Error::external)?;
            Ok(content)
        });

        // conv:ask(message) → returns { content, reasoning, length }
        methods.add_async_method("ask", |lua, this, message: String| async move {
            let (content, reasoning) = this
                .run_turn(&message)
                .await
                .map_err(mlua::Error::external)?;

            let table = lua.create_table()?;
            table.set("content", content)?;
            if let Some(r) = reasoning {
                table.set("reasoning", r)?;
            }
            table.set("length", this.messages.lock().await.len())?;
            Ok(table)
        });

        // conv:history() → list of {role, content}
        methods.add_async_method("history", |lua, this, ()| async move {
            let history = this.messages.lock().await;
            let table = lua.create_table()?;
            for (i, msg) in history.iter().enumerate() {
                let entry = lua.create_table()?;
                entry.set("role", msg.role.clone())?;
                if let Some(ref content) = msg.content {
                    entry.set("content", content.clone())?;
                }
                if let Some(ref tool_call_id) = msg.tool_call_id {
                    entry.set("tool_call_id", tool_call_id.clone())?;
                }
                table.set(i + 1, entry)?;
            }
            Ok(table)
        });

        // conv:length() → number of stored messages
        methods.add_async_method("length", |_, this, ()| async move {
            Ok(this.messages.lock().await.len())
        });

        // conv:reset() → clear all messages, keep the system prompt
        methods.add_async_method("reset", |_, this, ()| async move {
            let mut history = this.messages.lock().await;
            history.clear();
            history.push(ChatMessage::system(&this.system_prompt));
            Ok(())
        });

        // conv:agent_name() → the agent's name
        methods.add_method("agent_name", |_, this, ()| Ok(this.agent.name.clone()));

        // conv:id() → the stable session id (user-provided or auto-UUID)
        methods.add_method("id", |_, this, ()| Ok(this.id.clone()));

        // conv:is_persistent() → true if the session is tied to the store
        methods.add_method("is_persistent", |_, this, ()| Ok(this.persistent));

        // conv:save() → explicit save (useful when autosave = false)
        methods.add_async_method("save", |_, this, ()| async move {
            this.persist().await.map_err(mlua::Error::external)
        });

        // conv:delete() → remove the persisted record (and mark as non-persistent)
        methods.add_async_method("delete", |_, this, ()| async move {
            if let Some(ref store) = this.store {
                store
                    .delete_conversation(&this.id)
                    .await
                    .map_err(mlua::Error::external)?;
            }
            Ok(())
        });
    }
}

/// Build a LuaConversation from a Lua options table, agent lookup, provider,
/// tool registry, and crew defaults.
///
/// When `store` and the caller-provided `id` are both present, the conversation
/// is resumed from the store if a prior record exists. Autosave defaults to
/// `true` for persistent sessions and is a no-op for ephemeral ones.
#[allow(clippy::too_many_arguments)]
pub async fn build_conversation(
    table: Table,
    agents: &[Agent],
    provider: Arc<dyn LlmProvider>,
    tool_registry: ToolRegistry,
    crew_default_model: &str,
    crew_max_tool_rounds: usize,
    eventbus: EventBus,
    store: Option<Arc<dyn StateStore>>,
    flow_name: String,
) -> mlua::Result<LuaConversation> {
    // Resolve agent: either by name or inline (Agent table)
    let agent_value: Value = table.get("agent")?;
    let agent: Agent = match agent_value {
        Value::String(s) => {
            let name = s.to_str()?.to_string();
            agents
                .iter()
                .find(|a| a.name == name)
                .cloned()
                .ok_or_else(|| {
                    mlua::Error::external(IronCrewError::Validation(format!(
                        "Conversation: agent '{}' not found in crew",
                        name
                    )))
                })?
        }
        Value::Table(t) => crate::lua::parsers::agent_from_lua_table(&t)?,
        _ => {
            return Err(mlua::Error::external(IronCrewError::Validation(
                "Conversation requires 'agent' (string name or Agent table)".into(),
            )));
        }
    };

    let model: String = table
        .get::<String>("model")
        .ok()
        .or_else(|| agent.model.clone())
        .unwrap_or_else(|| crew_default_model.to_string());

    let system_prompt: String = table
        .get::<String>("system_prompt")
        .ok()
        .or_else(|| agent.system_prompt.clone())
        .unwrap_or_else(|| format!("You are {}. Your goal: {}", agent.name, agent.goal));

    // max_history resolution order:
    //   1. Explicit value in the Lua table (including 0 → unbounded)
    //   2. IRONCREW_CONVERSATION_MAX_HISTORY env var
    //   3. Safe default of 50 messages
    //
    // A value of 0 is treated as an explicit opt-in to unbounded history,
    // for backward compatibility with v2.3.x users who relied on unbounded.
    let max_history: Option<usize> = match table.get::<usize>("max_history") {
        Ok(0) => None, // explicit unbounded opt-in
        Ok(n) => Some(n),
        Err(_) => {
            let env_default = std::env::var("IRONCREW_CONVERSATION_MAX_HISTORY")
                .ok()
                .and_then(|v| v.parse::<usize>().ok());
            match env_default {
                Some(0) => None, // env var explicitly disables the cap
                Some(n) => Some(n),
                None => Some(50), // safe default
            }
        }
    };

    let stream: bool = table.get::<bool>("stream").unwrap_or(false);

    // Cross-run persistence: `id` is the persistence key. When omitted,
    // the session is ephemeral (same behavior as pre-2.8 conversations).
    let id: Option<String> = table.get::<String>("id").ok();
    // Autosave defaults to true when persistence is active. For non-persistent
    // sessions this value is effectively ignored.
    let autosave: bool = table.get::<bool>("autosave").unwrap_or(true);

    LuaConversation::new_or_resume(
        agent,
        provider,
        tool_registry,
        model,
        system_prompt,
        max_history,
        stream,
        crew_max_tool_rounds,
        eventbus,
        id,
        store,
        flow_name,
        autosave,
    )
    .await
    .map_err(mlua::Error::external)
}
