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
//! grab the `Arc` and call `run_turn().await` without a Lua VM round-trip.

mod persistence;
use std::sync::Arc;
mod bootstrap;
mod stream;

use mlua::{Lua, Table, UserData, UserDataMethods, Value};
use tokio::sync::{Mutex, OwnedMutexGuard};

use super::agent_turn::ActiveTurnGuard;
use crate::engine::agent::Agent;
use crate::engine::conversation_definition::{
    ConversationDefinition, conversation_definition_fingerprint,
};
use crate::engine::eventbus::{CrewEvent, EventBus};
use crate::engine::sessions::{ConversationExecution, ConversationRecord, validate_session_id};
use crate::engine::store::{ConversationCoordinationScope, StateStore};
use crate::llm::provider::{
    ChatMessage, ChatRequest, ChatResponse, DEFAULT_CHAT_HISTORY_MAX_MESSAGES,
    HARD_CHAT_HISTORY_MAX_MESSAGES, LlmProvider, StreamChunk, append_text_bounded,
    enforce_conversation_history_limits, validate_chat_history,
};
use crate::tools::ToolCallContext;
use crate::tools::registry::ToolRegistry;
use crate::utils::error::IronCrewError;

mod history_limit;
pub(crate) use history_limit::default_max_history;

/// Precomputed canonical flow identity injected by HTTP/CLI runtimes before
/// Lua executes. Keeping it in app data avoids a second blocking filesystem
/// walk and binds construction to the source snapshot already checked by the
/// caller.
#[derive(Clone)]
pub struct ConversationSourceFingerprint(pub String);

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
    /// Inclusive session receipts, including the last persisted checkpoint.
    usage: crate::usage::UsageTracker,

    /// Tool registry shared with the parent crew.
    pub tool_registry: ToolRegistry,

    /// Resolved model name.
    pub model: String,

    /// Effective system prompt (override or derived from the agent).
    pub system_prompt: String,

    /// Message history including the system prompt at index 0.
    pub messages: Mutex<Vec<ChatMessage>>,

    /// Serializes complete turn transactions independently from the messages
    /// snapshot. A turn executes against a private candidate history and only
    /// publishes it after durable persistence succeeds, so provider timeout,
    /// cancellation, or a lost database write leaves the visible transcript
    /// byte-for-byte unchanged.
    turn_execution_lock: Arc<Mutex<()>>,

    /// Optional cap on the number of stored messages (excluding system prompt).
    pub max_history: Option<usize>,

    /// Aggregate estimated footprint cap for the complete message history.
    pub history_max_bytes: usize,

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

    /// Revision expected by the next durable save. Held across persistence so
    /// overlapping save calls cannot reuse the same optimistic revision.
    pub revision: Mutex<u64>,

    /// Durable incarnation, definition identity, and original transcript
    /// limits represented by this in-memory cache.
    pub execution: ConversationExecution,
}

/// Candidate result of one conversation turn. The public conversation history
/// is intentionally untouched while this value exists. HTTP idempotency can
/// atomically persist `record` with its operation ledger before publishing the
/// candidate in memory.
pub struct PreparedConversationTurn {
    pub record: ConversationRecord,
    pub user_message: String,
    pub assistant: String,
    pub reasoning: Option<String>,
    pub turn_index: usize,
    pub turn_count: usize,
    _execution_guard: OwnedMutexGuard<()>,
}

impl LuaConversationInner {
    /// Session lifetime usage at this process checkpoint; never recharged to a new run.
    pub fn usage_snapshot(&self) -> Result<crate::usage::UsageSnapshot, IronCrewError> {
        crate::llm::scope::snapshot(&self.usage)
    }

    /// Reset history — clear all messages, keep the system prompt.
    pub async fn reset_history(&self) {
        let _execution_guard = self.turn_execution_lock.clone().lock_owned().await;
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

    /// Durable revision currently represented by the published in-memory
    /// transcript. HTTP idempotency claims fence their candidate turn against
    /// this value before any provider or tool call begins.
    pub async fn revision(&self) -> u64 {
        *self.revision.lock().await
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
        if self.persistent
            && self.store.as_ref().is_some_and(|store| {
                store.conversation_coordination_scope()
                    == ConversationCoordinationScope::SharedStore
            })
        {
            return Err(IronCrewError::Conflict(
                "Persistent conversations on a shared store require the keyed HTTP /messages endpoint so provider and tool work is durably fenced"
                    .into(),
            ));
        }
        let prepared = self
            .prepare_turn_with_ctx(user_message, images, caller_ctx)
            .await?;
        let new_revision = if self.autosave && self.persistent {
            let Some(store) = self.store.as_ref() else {
                return Err(IronCrewError::Validation(
                    "Persistent conversation has no state store".into(),
                ));
            };
            store.save_conversation(&prepared.record).await?
        } else {
            prepared.record.revision
        };
        self.publish_prepared_turn(prepared, new_revision).await
    }

    /// Execute a turn against a private transcript candidate. The visible
    /// in-memory history and durable conversation row are not mutated here.
    /// Callers must durably commit `prepared.record` and then invoke
    /// [`Self::publish_prepared_turn`], or simply drop the value to roll back
    /// without any transcript mutation.
    pub async fn prepare_turn_with_ctx(
        &self,
        user_message: &str,
        images: Option<Vec<crate::llm::provider::ImageInput>>,
        caller_ctx: &ToolCallContext,
    ) -> Result<PreparedConversationTurn, IronCrewError> {
        let execution_guard = self.turn_execution_lock.clone().lock_owned().await;
        let has_tools = !self.agent.tools.is_empty();
        let helper_ctx = ToolCallContext {
            usage_tracker: match &caller_ctx.usage_tracker {
                Some(caller) => Some(
                    caller
                        .child_observed_by(&self.usage)
                        .map_err(|error| IronCrewError::Provider(error.to_string()))?,
                ),
                None => self.provider.usage_tracker(),
            },
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
            ask_human: caller_ctx.ask_human.clone(),
        };

        // Work on a bounded private candidate. Keeping the published history
        // untouched is the transaction boundary for timeout/cancellation and
        // for persistence failures after provider/tool execution.
        let mut history = self.messages.lock().await.clone();
        let base_revision = *self.revision.lock().await;
        if let Some(imgs) = images {
            history.push(ChatMessage::user_with_images(user_message, imgs));
        } else {
            history.push(ChatMessage::user(user_message));
        }

        // 2. Streaming special case — preserve the original stream+no-tools
        //    path. The headless helper does not support streaming (Task 6
        //    scope), so when the caller opted into streaming and the agent
        //    has no tools, we keep the original inline branch.
        let (content, reasoning) = if self.stream && !has_tools {
            self.run_turn_streaming_no_tools(&mut history, helper_ctx.usage_tracker.clone())
                .await?
        } else {
            // 3. Non-streaming (or tools-present) path: delegate to the
            //    shared helper against the private candidate.
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

        if let Some(tracker) = &helper_ctx.usage_tracker {
            tracker.budget().check()?;
        }
        let turn_count = history.iter().filter(|m| m.role == "user").count();
        let turn_index = turn_count.saturating_sub(1);
        let record = ConversationRecord {
            usage: self.usage_snapshot()?,
            id: self.id.clone(),
            flow_name: self.flow_name.clone(),
            flow_path: self.flow_path.clone(),
            agent_name: self.agent.name.clone(),
            execution: self.execution.clone(),
            messages: history,
            created_at: self.created_at.clone(),
            updated_at: chrono::Utc::now().to_rfc3339(),
            revision: base_revision,
        };

        Ok(PreparedConversationTurn {
            record,
            user_message: user_message.to_string(),
            assistant: content,
            reasoning,
            turn_index,
            turn_count,
            _execution_guard: execution_guard,
        })
    }

    /// Publish a candidate only after the caller's durable commit succeeds.
    /// `new_revision` is the revision returned by the same store transaction.
    pub async fn publish_prepared_turn(
        &self,
        mut prepared: PreparedConversationTurn,
        new_revision: u64,
    ) -> Result<(String, Option<String>), IronCrewError> {
        let mut revision = self.revision.lock().await;
        if *revision != prepared.record.revision {
            return Err(IronCrewError::Conflict(format!(
                "Conversation '{}' changed while a prepared turn was pending",
                self.id
            )));
        }
        *self.messages.lock().await = std::mem::take(&mut prepared.record.messages);
        *revision = new_revision;
        drop(revision);

        self.eventbus.emit(CrewEvent::ConversationTurn {
            conversation_id: self.id.clone(),
            agent: self.agent.name.clone(),
            turn_index: prepared.turn_index,
            user_message: prepared.user_message,
            assistant_message: prepared.assistant.clone(),
        });

        if let Some(ref reasoning) = prepared.reasoning {
            self.eventbus.emit(CrewEvent::ConversationThinking {
                conversation_id: self.id.clone(),
                agent: self.agent.name.clone(),
                turn_index: prepared.turn_index,
                content: reasoning.clone(),
            });
        }

        Ok((prepared.assistant, prepared.reasoning))
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
        methods.add_method("usage", |lua, this, ()| {
            super::usage::snapshot(lua, &this.0.usage)
        });
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

mod construction;
pub use construction::build_conversation;
