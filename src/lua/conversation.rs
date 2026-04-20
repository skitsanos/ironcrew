//! LuaConversation — multi-turn chat with an agent.
//!
//! Created via `crew:conversation({...})`. Maintains its own message history
//! across `send()` / `ask()` calls. Supports tool calling via the crew's
//! tool registry, streaming to stderr, reasoning capture, and optional
//! cross-run persistence keyed by a stable `id`.
//!
//! The userdata is a thin handle around an `Arc<LuaConversationInner>`. All
//! state and behavior lives on the inner type; the outer struct only exists
//! so callers outside the Lua boundary (HTTP handlers, CLI `chat` REPL) can
//! grab a clone of the `Arc` and call `run_turn().await` directly without
//! bouncing back through the Lua VM.

use std::sync::Arc;

use mlua::{Table, UserData, UserDataMethods, Value};
use tokio::sync::Mutex;

use crate::engine::agent::Agent;
use crate::engine::eventbus::{CrewEvent, EventBus};
use crate::engine::sessions::{ConversationRecord, validate_session_id};
use crate::engine::store::StateStore;
use crate::llm::provider::{ChatMessage, ChatRequest, ChatResponse, LlmProvider, StreamChunk};
use crate::tools::ToolCallContext;
use crate::tools::registry::ToolRegistry;
use crate::utils::error::IronCrewError;

/// Resolve the default max_history cap when no explicit Lua-side value is
/// provided. Honors `IRONCREW_CONVERSATION_MAX_HISTORY` (0 → unbounded),
/// falling back to a safe 50-message cap. Shared with non-conversation
/// consumers (e.g. `AgentAsTool` finalization) so they apply the same
/// policy as the user-facing `crew:conversation()` path.
pub(crate) fn default_max_history() -> Option<usize> {
    let env_default = std::env::var("IRONCREW_CONVERSATION_MAX_HISTORY")
        .ok()
        .and_then(|v| v.parse::<usize>().ok());
    match env_default {
        Some(0) => None, // env var explicitly disables the cap
        Some(n) => Some(n),
        None => Some(50), // safe default
    }
}

/// Shared inner state of a conversation. All methods live here so that
/// non-Lua consumers (HTTP API, CLI REPL) can call them via
/// `Arc<LuaConversationInner>` without a Lua round-trip.
pub struct LuaConversationInner {
    /// Stable identifier — included in every SSE event for this conversation.
    /// If the user provided one via `id = "..."`, it's the persistence key;
    /// otherwise it's an auto-UUID and the session is not persisted.
    pub id: String,

    /// Project directory for resolving relative image paths.
    pub project_dir: std::path::PathBuf,

    /// Shared HTTP client for downloading image URLs.
    pub http_client: reqwest::Client,

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

    /// User-facing flow path (e.g. "chat-cli") used by the HTTP/CLI layer
    /// to group sessions. Falls back to `None` for Lua-only callers that
    /// didn't thread a flow path through — those records stay addressable
    /// by id but won't appear in per-flow listings.
    pub flow_path: Option<String>,

    /// When `true` (default for persistent sessions), the conversation is
    /// auto-saved to the store after every completed turn. Opt out with
    /// `autosave = false` and call `conversation:save()` manually.
    pub autosave: bool,

    /// RFC3339 timestamp of the original creation (loaded from the store on
    /// resume, or set at construction for fresh sessions).
    pub created_at: String,
}

impl LuaConversationInner {
    /// Build a fresh (or resumed) conversation inner.
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
        flow_path: Option<String>,
        autosave: bool,
        project_dir: std::path::PathBuf,
        http_client: reqwest::Client,
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
            && let Some(record) = store.get_conversation(flow_path.as_deref(), &id).await?
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
            flow_path,
            autosave,
            created_at,
            project_dir,
            http_client,
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
            flow_path: self.flow_path.clone(),
            agent_name: self.agent.name.clone(),
            messages,
            created_at: self.created_at.clone(),
            updated_at: chrono::Utc::now().to_rfc3339(),
        };
        store.save_conversation(&record).await
    }

    /// Reset history — clear all messages, keep the system prompt.
    pub async fn reset_history(&self) {
        let mut history = self.messages.lock().await;
        history.clear();
        history.push(ChatMessage::system(&self.system_prompt));
    }

    /// Current number of messages (including system prompt).
    pub async fn message_count(&self) -> usize {
        self.messages.lock().await.len()
    }

    /// Snapshot of the full message list.
    pub async fn messages_snapshot(&self) -> Vec<ChatMessage> {
        self.messages.lock().await.clone()
    }

    /// Number of user turns completed so far.
    pub async fn turn_count(&self) -> usize {
        self.messages
            .lock()
            .await
            .iter()
            .filter(|m| m.role == "user")
            .count()
    }

    /// Run a single send/respond round (with tool-call loop) and return the
    /// assistant text plus any reasoning captured across tool rounds.
    ///
    /// Thin wrapper over `run_turn_with_ctx` that supplies a default
    /// (empty) caller context — the conversation's own `store`, `eventbus`,
    /// and `tool_registry` fill in the helper's `ToolCallContext`.
    pub async fn run_turn(
        &self,
        user_message: &str,
        images: Option<Vec<crate::llm::provider::ImageInput>>,
    ) -> Result<(String, Option<String>), IronCrewError> {
        self.run_turn_with_ctx(user_message, images, &ToolCallContext::default())
            .await
    }

    /// `run_turn` variant that threads an explicit `ToolCallContext`
    /// through to the shared single-turn helper. Used by callers
    /// (agent-as-tool, nested sub-flows) that already have
    /// `depth` / `caller_scope` / `caller_agent` populated and want those
    /// values preserved on any nested dispatch this turn triggers.
    ///
    /// `caller_ctx` semantics — per field, the helper context is built as
    /// "caller's value, falling back to the conversation's own":
    ///   * `store`          — caller override, else `self.store`
    ///   * `eventbus`       — caller override, else `self.eventbus`
    ///   * `tool_registry`  — caller override, else `self.tool_registry`
    ///   * `depth`          — taken from caller (unchanged)
    ///   * `caller_scope`   — taken from caller, else `self.id`
    ///   * `caller_agent`   — always this conversation's agent name
    pub async fn run_turn_with_ctx(
        &self,
        user_message: &str,
        images: Option<Vec<crate::llm::provider::ImageInput>>,
        caller_ctx: &ToolCallContext,
    ) -> Result<(String, Option<String>), IronCrewError> {
        // 1. Append the user message to history.
        {
            let mut history = self.messages.lock().await;
            if let Some(imgs) = images {
                history.push(ChatMessage::user_with_images(user_message, imgs));
            } else {
                history.push(ChatMessage::user(user_message));
            }
            self.enforce_history_cap(&mut history);
        }

        let has_tools = !self.agent.tools.is_empty();

        // 2. Streaming special case — preserve the original stream+no-tools
        //    path. The headless helper does not support streaming (Task 6
        //    scope), so when the caller opted into streaming and the agent
        //    has no tools, we keep the original inline branch.
        let (content, reasoning) = if self.stream && !has_tools {
            self.run_turn_streaming_no_tools().await?
        } else {
            // 3. Non-streaming (or tools-present) path: delegate to the
            //    shared helper under the history lock. The lock is held
            //    across the helper's awaits on purpose — `self.messages`
            //    has a single writer per turn, and releasing it mid-turn
            //    would let a concurrent caller interleave user messages.
            //    `tokio::sync::Mutex` is await-safe so this is fine.
            let helper_ctx = ToolCallContext {
                store: caller_ctx.store.clone().or_else(|| self.store.clone()),
                eventbus: Some(
                    caller_ctx
                        .eventbus
                        .clone()
                        .unwrap_or_else(|| self.eventbus.clone()),
                ),
                depth: caller_ctx.depth,
                tool_registry: Some(
                    caller_ctx
                        .tool_registry
                        .clone()
                        .unwrap_or_else(|| self.tool_registry.clone()),
                ),
                caller_agent: Some(self.agent.name.clone()),
                caller_scope: Some(
                    caller_ctx
                        .caller_scope
                        .clone()
                        .unwrap_or_else(|| self.id.clone()),
                ),
            };

            let mut history = self.messages.lock().await;
            crate::lua::agent_turn::run_single_agent_turn(
                &self.agent,
                &self.provider,
                &self.model,
                self.max_tool_rounds,
                self.max_history,
                &mut history,
                &helper_ctx,
            )
            .await?
        };

        // 4. Compute turn index and emit conversation lifecycle events.
        //    `turn_index` is 0-based: the number of user messages already
        //    in history (including the one we just pushed) minus 1.
        let turn_index = {
            let history = self.messages.lock().await;
            history
                .iter()
                .filter(|m| m.role == "user")
                .count()
                .saturating_sub(1)
        };

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

        // 5. Autosave after each successful turn (no-op for sessions
        //    without a store or with autosave disabled).
        if self.autosave
            && self.persistent
            && let Err(e) = self.persist().await
        {
            tracing::warn!("Autosave failed for conversation '{}': {}", self.id, e);
        }

        Ok((content, reasoning))
    }

    /// Streaming no-tools turn. The user message has already been pushed
    /// by the caller; this method issues one streaming provider call,
    /// appends the assistant reply, and returns (content, reasoning).
    ///
    /// Tool-call rounds are not supported here by design — the shared
    /// helper owns that path and does not stream. Callers must check
    /// `has_tools` before dispatching to this method.
    async fn run_turn_streaming_no_tools(&self) -> Result<(String, Option<String>), IronCrewError> {
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

        let response = self.call_streaming(request).await?;

        let content = response
            .content
            .ok_or_else(|| IronCrewError::Provider("Empty response from LLM".into()))?;

        {
            let mut history = self.messages.lock().await;
            history.push(ChatMessage::assistant(Some(content.clone()), None));
            self.enforce_history_cap(&mut history);
        }

        Ok((content, response.reasoning))
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

    /// Delete the persisted record (if any) for this session. Flow-scoped
    /// so a conversation can only delete its own flow's record.
    pub async fn delete(&self) -> Result<(), IronCrewError> {
        if let Some(ref store) = self.store {
            store
                .delete_conversation(self.flow_path.as_deref(), &self.id)
                .await?;
        }
        Ok(())
    }
}

/// Lua userdata wrapper. Holds an `Arc<LuaConversationInner>` so callers
/// outside Lua can share ownership without duplicating state.
#[derive(Clone)]
pub struct LuaConversation(pub Arc<LuaConversationInner>);

impl LuaConversation {
    /// Clone the underlying `Arc` so other components (HTTP handlers, CLI
    /// REPL) can call `run_turn()` directly without going through Lua.
    pub fn inner(&self) -> Arc<LuaConversationInner> {
        Arc::clone(&self.0)
    }
}

impl UserData for LuaConversation {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        // conv:send(message[, opts]) → returns plain text
        // opts may include: { images = { "path/to/img.png", "https://..." } }
        methods.add_async_method("send", |_, this, args: mlua::MultiValue| async move {
            let mut args_iter = args.into_iter();

            let message: String = match args_iter.next() {
                Some(mlua::Value::String(s)) => s.to_str()?.to_string(),
                _ => {
                    return Err(mlua::Error::external(
                        crate::utils::error::IronCrewError::Validation(
                            "send() requires a string message as first argument".into(),
                        ),
                    ));
                }
            };

            let images =
                parse_images_from_opts(args_iter.next(), &this.0.project_dir, &this.0.http_client)
                    .await?;

            let (content, _reasoning) = this
                .0
                .run_turn(&message, images)
                .await
                .map_err(mlua::Error::external)?;
            Ok(content)
        });

        // conv:ask(message[, opts]) → returns { content, reasoning, length }
        // opts may include: { images = { "path/to/img.png", "https://..." } }
        methods.add_async_method("ask", |lua, this, args: mlua::MultiValue| async move {
            let mut args_iter = args.into_iter();

            let message: String = match args_iter.next() {
                Some(mlua::Value::String(s)) => s.to_str()?.to_string(),
                _ => {
                    return Err(mlua::Error::external(
                        crate::utils::error::IronCrewError::Validation(
                            "ask() requires a string message as first argument".into(),
                        ),
                    ));
                }
            };

            let images =
                parse_images_from_opts(args_iter.next(), &this.0.project_dir, &this.0.http_client)
                    .await?;

            let (content, reasoning) = this
                .0
                .run_turn(&message, images)
                .await
                .map_err(mlua::Error::external)?;

            let table = lua.create_table()?;
            table.set("content", content)?;
            if let Some(r) = reasoning {
                table.set("reasoning", r)?;
            }
            table.set("length", this.0.message_count().await)?;
            Ok(table)
        });

        // conv:history() → list of {role, content}
        methods.add_async_method("history", |lua, this, ()| async move {
            let history = this.0.messages.lock().await;
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
            Ok(this.0.message_count().await)
        });

        // conv:reset() → clear all messages, keep the system prompt
        methods.add_async_method("reset", |_, this, ()| async move {
            this.0.reset_history().await;
            Ok(())
        });

        // conv:agent_name() → the agent's name
        methods.add_method("agent_name", |_, this, ()| Ok(this.0.agent.name.clone()));

        // conv:id() → the stable session id (user-provided or auto-UUID)
        methods.add_method("id", |_, this, ()| Ok(this.0.id.clone()));

        // conv:is_persistent() → true if the session is tied to the store
        methods.add_method("is_persistent", |_, this, ()| Ok(this.0.persistent));

        // conv:save() → explicit save (useful when autosave = false)
        methods.add_async_method("save", |_, this, ()| async move {
            this.0.persist().await.map_err(mlua::Error::external)
        });

        // conv:delete() → remove the persisted record (and mark as non-persistent)
        methods.add_async_method("delete", |_, this, ()| async move {
            this.0.delete().await.map_err(mlua::Error::external)
        });
    }
}

/// Parse an optional `{ images = { ... } }` table from Lua into a loaded
/// `Vec<ImageInput>`. Returns `None` when no images are present or the
/// argument is absent / not a table.
async fn parse_images_from_opts(
    opts_value: Option<mlua::Value>,
    project_dir: &std::path::Path,
    client: &reqwest::Client,
) -> mlua::Result<Option<Vec<crate::llm::provider::ImageInput>>> {
    match opts_value {
        Some(mlua::Value::Table(opts)) => {
            if let Ok(img_table) = opts.get::<mlua::Table>("images") {
                // Collect paths before any await so the non-Send iterator
                // is dropped before we cross async boundaries.
                let paths: Vec<String> = img_table
                    .sequence_values::<String>()
                    .collect::<mlua::Result<Vec<_>>>()?;

                let mut loaded = Vec::new();
                for path in paths {
                    let img = crate::llm::image::load_image(&path, project_dir, client)
                        .await
                        .map_err(mlua::Error::external)?;
                    loaded.push(img);
                }
                if loaded.is_empty() {
                    Ok(None)
                } else {
                    Ok(Some(loaded))
                }
            } else {
                Ok(None)
            }
        }
        _ => Ok(None),
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
    flow_path: Option<String>,
    project_dir: std::path::PathBuf,
    http_client: reqwest::Client,
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
        Err(_) => default_max_history(),
    };

    let stream: bool = table.get::<bool>("stream").unwrap_or(false);

    // Cross-run persistence: `id` is the persistence key. When omitted,
    // the session is ephemeral (same behavior as pre-2.8 conversations).
    let id: Option<String> = table.get::<String>("id").ok();
    // Autosave defaults to true when persistence is active. For non-persistent
    // sessions this value is effectively ignored.
    //
    // NOTE: use `Option<bool>` rather than `bool` here — `table.get::<bool>`
    // on a missing key coerces nil to `false` (mlua's FromLua impl), which
    // would silently disable autosave whenever the caller omits the field.
    let autosave: bool = table
        .get::<Option<bool>>("autosave")
        .ok()
        .flatten()
        .unwrap_or(true);

    let inner = LuaConversationInner::new_or_resume(
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
        flow_path,
        autosave,
        project_dir,
        http_client,
    )
    .await
    .map_err(mlua::Error::external)?;

    Ok(LuaConversation(Arc::new(inner)))
}
