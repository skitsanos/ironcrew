use std::path::PathBuf;
use std::sync::Arc;

use mlua::{Table, UserData, UserDataMethods, Value};
use tokio::sync::{Mutex, OnceCell};

use crate::engine::crew::Crew;
use crate::engine::eventbus::EventBus;
use crate::engine::messagebus::{Message, MessageType};
use crate::engine::runtime::Runtime;
use crate::engine::store::{StateStore, create_store};
use crate::llm::provider::LlmProvider;
use crate::tools::agent_as_tool::AgentAsTool;
use crate::utils::error::{IronCrewError, Result};

#[cfg(feature = "mcp")]
use crate::mcp::{McpConfig, McpConnectionManager};

use super::conversation::build_conversation;
use super::dialog::build_dialog;
use super::json::{json_value_to_lua, lua_table_to_json, lua_value_to_json};
use super::parsers::{agent_from_lua_table, task_from_lua_table};
use super::subflow::{SubflowContext, SubflowDepth, invoke_subflow};

// ---------------------------------------------------------------------------
// LuaCrew — Lua userdata wrapping a Crew + Runtime
// ---------------------------------------------------------------------------

/// Wrapper holding crew + runtime for Lua userdata.
/// If the Lua Crew.new() specifies a custom api_key or base_url,
/// a per-crew provider overrides the runtime's default provider.
pub struct LuaCrew {
    pub crew: Arc<Mutex<Crew>>,
    pub runtime: Arc<Runtime>,
    pub custom_provider: Option<Arc<dyn LlmProvider>>,
    pub project_dir: PathBuf,
    /// Lazily-initialized shared store. The first call to
    /// `get_or_init_store()` creates the backing `StateStore` for this
    /// crew; subsequent `crew:run()`, `crew:conversation()`, and
    /// `crew:dialog()` calls all reuse the same instance. This matters
    /// most for PostgreSQL, where each `create_store()` call would
    /// otherwise spin up a fresh connection pool.
    pub store: OnceCell<Arc<dyn StateStore>>,
    /// Parsed MCP server configuration (set at Crew.new() time).
    #[cfg(feature = "mcp")]
    pub mcp_config: Option<McpConfig>,
    /// Cached MCP connection manager. Created once on the first `crew:run()` call
    /// and reused for all subsequent runs (no reconnect overhead).
    #[cfg(feature = "mcp")]
    pub mcp_manager: Arc<Mutex<Option<McpConnectionManager>>>,
    /// Augmented tool registry that includes MCP tools. Set after first connect_all.
    /// Stored here so subsequent runs don't re-register MCP tools on each call.
    #[cfg(feature = "mcp")]
    pub mcp_tool_registry: Arc<Mutex<Option<crate::tools::registry::ToolRegistry>>>,
    /// Lazy-finalized agent-as-tool registry. Populated on first
    /// entry-point call (run / conversation / dialog / chat). Caches
    /// both Ok and Err results — the same bad config always produces
    /// the same error without re-running validation. To fix a bad
    /// config, construct a fresh Crew via Crew.new().
    #[allow(dead_code)] // wired into entry points in Tasks 11 & 12
    pub(crate) agent_tools_finalized:
        OnceCell<std::result::Result<Arc<FinalizedAgentTools>, String>>,
}

impl LuaCrew {
    /// Lazily create (or return the cached) shared `StateStore` for this crew.
    async fn get_or_init_store(&self) -> Result<Arc<dyn StateStore>> {
        let ironcrew_dir = self.project_dir.join(".ironcrew");
        let store = self
            .store
            .get_or_try_init(|| async { create_store(ironcrew_dir).await })
            .await?;
        Ok(store.clone())
    }

    /// Ensure agent-as-tool finalization has run for this crew.
    /// The result is cached: a second call returns the same
    /// `Arc<FinalizedAgentTools>` on success, or the same error
    /// string (wrapped as IronCrewError::Validation) on failure.
    #[allow(dead_code)] // called by entry points in Tasks 11 & 12
    pub(crate) async fn ensure_agent_tools_finalized(
        &self,
    ) -> std::result::Result<Arc<FinalizedAgentTools>, IronCrewError> {
        let result = self
            .agent_tools_finalized
            .get_or_init(|| async { finalize_agent_tools(self).await.map_err(|e| e.to_string()) })
            .await;
        match result {
            Ok(ft) => Ok(ft.clone()),
            Err(msg) => Err(IronCrewError::Validation(msg.clone())),
        }
    }
}

// ---------------------------------------------------------------------------
// Agent-as-tool finalization
// ---------------------------------------------------------------------------

/// Snapshot of a crew's agent-as-tool registrations plus the per-crew
/// defaults captured at finalization time. Built once per `LuaCrew` on the
/// first entry-point call (crew:run / :conversation / :dialog / :chat) and
/// then reused for the lifetime of the crew.
pub(crate) struct FinalizedAgentTools {
    /// Augmented registry = built-ins (+ MCP if present) + one
    /// `AgentAsTool` per distinct `agent__<name>` reference found across
    /// the crew's agent tool lists.
    #[allow(dead_code)] // consumed by entry points in Tasks 11 & 12
    pub registry: crate::tools::registry::ToolRegistry,
    /// Crew-wide default model, captured so per-`AgentAsTool` callers
    /// don't need to re-lock the crew to find it.
    #[allow(dead_code)] // captured for observability / future use
    pub default_model: String,
    /// Crew-wide tool-round cap, mirrored into each `AgentAsTool`.
    #[allow(dead_code)]
    pub max_tool_rounds: usize,
    /// Resolved conversation history cap (see
    /// `conversation::default_max_history`).
    #[allow(dead_code)]
    pub max_history: Option<usize>,
}

/// Finalize agent-as-tool registrations for a crew. Runs once per
/// `LuaCrew` on first entry-point call. Returns an augmented tool
/// registry containing built-ins + MCP (if present) + one `AgentAsTool`
/// per distinct `agent__<name>` reference across all agents' tool lists.
///
/// Errors if a reference points at an unknown agent name — this is a
/// crew authoring bug, not a runtime failure, so we surface it eagerly
/// rather than letting the LLM discover it at tool-call time.
#[allow(dead_code)] // called by ensure_agent_tools_finalized, wired in Tasks 11 & 12
pub(crate) async fn finalize_agent_tools(
    lua_crew: &LuaCrew,
) -> std::result::Result<Arc<FinalizedAgentTools>, IronCrewError> {
    // 1. Snapshot everything we need off the crew under a short lock.
    let (agents, default_model, max_tool_rounds) = {
        let crew_guard = lua_crew.crew.lock().await;
        (
            crew_guard.agents.clone(),
            crew_guard.provider_config.model.clone(),
            crew_guard.max_tool_rounds,
        )
    };
    let max_history = crate::lua::conversation::default_max_history();

    // 2. Collect distinct `agent__<name>` suffixes across every agent's
    //    tools list. BTreeSet gives stable ordering for deterministic
    //    registration and cheap dedup.
    let mut needed: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for a in &agents {
        for t in &a.tools {
            if let Some(suffix) = t.strip_prefix("agent__") {
                needed.insert(suffix.to_string());
            }
        }
    }

    // 3. Start with the base registry. If MCP is compiled in and the
    //    augmented registry has already been built, layer on top of that
    //    instead of the bare runtime registry — agent tools should see
    //    MCP tools too. If MCP is configured but not yet connected,
    //    connect all servers now and cache the augmented registry +
    //    manager on the LuaCrew for reuse.
    #[cfg(feature = "mcp")]
    let base_registry = {
        let cached = {
            let guard = lua_crew.mcp_tool_registry.lock().await;
            guard.clone()
        };
        if let Some(registry) = cached {
            registry
        } else if let Some(ref mcp_cfg) = lua_crew.mcp_config {
            if mcp_cfg.is_empty() {
                lua_crew.runtime.tool_registry.clone()
            } else {
                let mut registry = lua_crew.runtime.tool_registry.clone();
                let manager = McpConnectionManager::connect_all(mcp_cfg, &mut registry).await?;
                {
                    let mut guard = lua_crew.mcp_manager.lock().await;
                    *guard = Some(manager);
                }
                {
                    let mut guard = lua_crew.mcp_tool_registry.lock().await;
                    *guard = Some(registry.clone());
                }
                registry
            }
        } else {
            lua_crew.runtime.tool_registry.clone()
        }
    };
    #[cfg(not(feature = "mcp"))]
    let base_registry = lua_crew.runtime.tool_registry.clone();

    let mut registry = base_registry;

    // 4. Resolve provider + project_dir + weak runtime handle once.
    let provider: Arc<dyn LlmProvider> = match &lua_crew.custom_provider {
        Some(p) => p.clone(),
        None => lua_crew.runtime.provider.clone(),
    };
    let project_dir = Arc::new(lua_crew.project_dir.clone());

    // Obtain `Weak<Runtime>` by upgrading the runtime's stored self-ref
    // and re-downgrading. `set_self_ref` is called from
    // `setup_crew_runtime`; if we get here without it, something is
    // wrong with bootstrap and we refuse to finalize.
    let runtime_arc = lua_crew.runtime.upgrade_self().ok_or_else(|| {
        IronCrewError::Validation(
            "Agent-as-tool: Runtime self-ref not initialized (set_self_ref \
             was not called after Arc::new); cannot finalize agent tools"
                .into(),
        )
    })?;
    let runtime_weak: std::sync::Weak<Runtime> = Arc::downgrade(&runtime_arc);
    drop(runtime_arc);

    // 5. Build one `AgentAsTool` per distinct callee, validating that
    //    each referenced name resolves to an actual agent.
    for callee in &needed {
        let agent = agents.iter().find(|a| &a.name == callee).ok_or_else(|| {
            IronCrewError::Validation(format!(
                "Agent-as-tool: unknown agent '{}' referenced in a tools list \
                 (as 'agent__{}'); no agent by that name was added to the crew",
                callee, callee
            ))
        })?;

        let resolved_model = agent.model.clone().unwrap_or_else(|| default_model.clone());
        let tool = AgentAsTool::new(
            agent.clone(),
            provider.clone(),
            runtime_weak.clone(),
            resolved_model,
            max_tool_rounds,
            max_history,
            project_dir.clone(),
        );
        registry.register_arc(Arc::new(tool));
    }

    Ok(Arc::new(FinalizedAgentTools {
        registry,
        default_model,
        max_tool_rounds,
        max_history,
    }))
}

impl UserData for LuaCrew {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        // Use add_async_method for all methods to avoid block_on inside Tokio
        methods.add_async_method("add_agent", |_, this, table: Table| async move {
            let agent = agent_from_lua_table(&table)?;
            let agent_name = agent.name.clone();

            let mut crew = this.crew.lock().await;

            // Extract before_task hook if present and store as bytecode
            if let Ok(func) = table.get::<mlua::Function>("before_task") {
                let bytecode = func.dump(false);
                crew.before_task_hooks.insert(agent_name.clone(), bytecode);
            }

            // Extract after_task hook if present and store as bytecode
            if let Ok(func) = table.get::<mlua::Function>("after_task") {
                let bytecode = func.dump(false);
                crew.after_task_hooks.insert(agent_name.clone(), bytecode);
            }

            crew.add_agent(agent);
            Ok(())
        });

        methods.add_async_method("add_task", |_, this, table: Table| async move {
            let task = task_from_lua_table(&table)?;
            this.crew.lock().await.add_task(task);
            Ok(())
        });

        methods.add_async_method(
            "add_task_if",
            |_, this, (condition, table): (String, Table)| async move {
                let mut task = task_from_lua_table(&table)?;
                task.condition = Some(condition);
                this.crew.lock().await.add_task(task);
                Ok(())
            },
        );

        methods.add_async_method(
            "subworkflow",
            |lua, this, (path, options): (String, Option<Table>)| async move {
                // Parse options — `output_key` wraps the result in a single-field
                // table, `input` gets JSON-bridged to the sub-flow's global `input`.
                let output_key: Option<String> =
                    options.as_ref().and_then(|o| o.get("output_key").ok());
                let input_table: Option<Table> = options.as_ref().and_then(|o| o.get("input").ok());
                let input_json: Option<serde_json::Value> = match input_table {
                    Some(ref t) => Some(lua_table_to_json(t)?),
                    None => None,
                };

                // Current depth lives on the parent VM's app-data; when this
                // method is called from top-level crew.lua it's 0.
                let depth = lua.app_data_ref::<SubflowDepth>().map(|d| d.0).unwrap_or(0);
                // Prefer the API-injected EventBus (keeps SSE events flowing
                // through the same channel as the parent crew).
                let eventbus = lua.app_data_ref::<EventBus>().map(|e| e.clone());

                let ctx = SubflowContext {
                    runtime: this.runtime.clone(),
                    project_dir: Arc::new(this.project_dir.clone()),
                    depth,
                    eventbus,
                    output_key,
                };

                invoke_subflow(&lua, path, input_json, &ctx).await
            },
        );

        // Foreach task method
        methods.add_async_method("add_foreach_task", |_, this, table: Table| async move {
            let task = task_from_lua_table(&table)?;
            if task.foreach_source.is_none() {
                return Err(mlua::Error::external(IronCrewError::Validation(
                    "foreach task requires 'foreach' field specifying the source key".into(),
                )));
            }
            this.crew.lock().await.add_task(task);
            Ok(())
        });

        // Collaborative task method
        methods.add_async_method(
            "add_collaborative_task",
            |_, this, table: Table| async move {
                let mut task = task_from_lua_table(&table)?;
                task.task_type = Some("collaborative".into());

                if task.collaborative_agents.len() < 2 {
                    return Err(mlua::Error::external(IronCrewError::Validation(
                        "Collaborative task requires 'agents' field with at least 2 agent names"
                            .into(),
                    )));
                }

                this.crew.lock().await.add_task(task);
                Ok(())
            },
        );

        // MessageBus methods
        methods.add_async_method(
            "message_send",
            |_, this, (from, to, content, msg_type): (String, String, String, Option<String>)| async move {
                let message_type = match msg_type.as_deref() {
                    Some("request") => MessageType::Request,
                    Some("broadcast") => MessageType::Broadcast,
                    _ => MessageType::Notification,
                };
                let message = Message::new(from, to, content, message_type);
                this.crew.lock().await.messagebus.send(message).await;
                Ok(())
            },
        );

        methods.add_async_method("message_read", |lua, this, agent_name: String| async move {
            let messages = this.crew.lock().await.messagebus.receive(&agent_name).await;
            let table = lua.create_table()?;
            for (i, msg) in messages.iter().enumerate() {
                let entry = lua.create_table()?;
                entry.set("id", msg.id.clone())?;
                entry.set("from", msg.from.clone())?;
                entry.set("to", msg.to.clone())?;
                entry.set("content", msg.content.clone())?;
                entry.set("type", format!("{:?}", msg.message_type))?;
                entry.set("timestamp", msg.timestamp)?;
                table.set(i + 1, entry)?;
            }
            Ok(table)
        });

        methods.add_async_method("message_history", |lua, this, ()| async move {
            let history = this.crew.lock().await.messagebus.get_history().await;
            let table = lua.create_table()?;
            for (i, msg) in history.iter().enumerate() {
                let entry = lua.create_table()?;
                entry.set("from", msg.from.clone())?;
                entry.set("to", msg.to.clone())?;
                entry.set("content", msg.content.clone())?;
                entry.set("type", format!("{:?}", msg.message_type))?;
                table.set(i + 1, entry)?;
            }
            Ok(table)
        });

        // Memory methods
        methods.add_async_method(
            "memory_set",
            |_, this, (key, value): (String, Value)| async move {
                let json_value = lua_value_to_json(value)?;
                this.crew.lock().await.memory.set(key, json_value).await;
                Ok(())
            },
        );

        methods.add_async_method(
            "memory_set_ex",
            |_, this, (key, value, options): (String, Value, Table)| async move {
                let json_value = lua_value_to_json(value)?;
                let tags: Vec<String> = options
                    .get::<Table>("tags")
                    .map(|t| {
                        t.sequence_values::<String>()
                            .filter_map(|v| v.ok())
                            .collect()
                    })
                    .unwrap_or_default();
                let ttl_ms: Option<i64> = options.get("ttl_ms").ok();
                this.crew
                    .lock()
                    .await
                    .memory
                    .set_with_options(key, json_value, tags, ttl_ms)
                    .await;
                Ok(())
            },
        );

        methods.add_async_method("memory_get", |lua, this, key: String| async move {
            let value = this.crew.lock().await.memory.get(&key).await;
            match value {
                Some(v) => json_value_to_lua(&lua, &v),
                None => Ok(Value::Nil),
            }
        });

        methods.add_async_method("memory_delete", |_, this, key: String| async move {
            Ok(this.crew.lock().await.memory.delete(&key).await)
        });

        methods.add_async_method("memory_keys", |lua, this, ()| async move {
            let keys = this.crew.lock().await.memory.keys().await;
            let table = lua.create_table()?;
            for (i, key) in keys.iter().enumerate() {
                table.set(i + 1, key.as_str())?;
            }
            Ok(table)
        });

        methods.add_async_method("memory_clear", |_, this, ()| async move {
            this.crew.lock().await.memory.clear().await;
            Ok(())
        });

        methods.add_async_method("memory_stats", |lua, this, ()| async move {
            let stats = this.crew.lock().await.memory.stats().await;
            let table = lua.create_table()?;
            table.set("total_items", stats.total_items)?;
            table.set("total_tokens", stats.total_tokens)?;
            Ok(table)
        });

        // crew:conversation({agent = ..., model = ..., stream = ..., ...})
        // Creates a stateful multi-turn conversation bound to this crew.
        methods.add_async_method("conversation", |lua, this, table: Table| async move {
            let crew = this.crew.lock().await;
            let provider: Arc<dyn LlmProvider> = match &this.custom_provider {
                Some(p) => p.clone(),
                None => this.runtime.provider.clone(),
            };
            let agents: Vec<crate::engine::agent::Agent> = crew.agents.clone();
            let default_model = crew.provider_config.model.clone();
            let max_tool_rounds = crew.max_tool_rounds;
            let flow_name = crew.goal.clone();
            // Prefer API-injected EventBus (from app_data) so events flow through
            // the same SSE channel as task events; fall back to the crew's bus.
            let eventbus = lua
                .app_data_ref::<EventBus>()
                .map(|e| e.clone())
                .unwrap_or_else(|| crew.eventbus.clone());
            drop(crew);

            // Resolve the shared store for cross-run persistence. If the
            // store can't be created (e.g. misconfigured PG URL), fall back
            // to an ephemeral in-memory session rather than failing the
            // whole call — this mirrors how ephemeral conversations worked
            // before the persistence layer was added.
            let store = this.get_or_init_store().await.ok();

            // Derive flow_path from the project directory's last segment
            // so records created via the Lua-only path can still be looked
            // up via the HTTP/CLI per-flow list views.
            let flow_path = this
                .project_dir
                .file_name()
                .and_then(|s| s.to_str())
                .map(|s| s.to_string());

            let conv = build_conversation(
                table,
                &agents,
                provider,
                this.runtime.tool_registry.clone(),
                &default_model,
                max_tool_rounds,
                eventbus,
                store,
                flow_name,
                flow_path,
                this.project_dir.clone(),
                reqwest::Client::new(),
            )
            .await?;
            Ok(conv)
        });

        // crew:dialog({agents = {"name", ...}, starter = ..., ...})
        // Creates an agent-to-agent dialog with perspective-flipped histories.
        methods.add_async_method("dialog", |lua, this, table: Table| async move {
            let crew = this.crew.lock().await;
            let provider: Arc<dyn LlmProvider> = match &this.custom_provider {
                Some(p) => p.clone(),
                None => this.runtime.provider.clone(),
            };
            let agents: Vec<crate::engine::agent::Agent> = crew.agents.clone();
            let default_model = crew.provider_config.model.clone();
            let max_tool_rounds = crew.max_tool_rounds;
            let flow_name = crew.goal.clone();
            let eventbus = lua
                .app_data_ref::<EventBus>()
                .map(|e| e.clone())
                .unwrap_or_else(|| crew.eventbus.clone());
            drop(crew);

            // Shared store (same pattern as conversation) — falls back to
            // ephemeral mode if the store can't be created.
            let store = this.get_or_init_store().await.ok();

            // Mirror the conversation path: derive the flow_path slug from
            // the project directory so dialog records are namespaced by
            // flow. Without this, dialogs would still save with
            // `flow_path = None` and cross-flow id collisions would be
            // possible.
            let flow_path = this
                .project_dir
                .file_name()
                .and_then(|s| s.to_str())
                .map(|s| s.to_string());

            let dialog = build_dialog(
                &lua,
                table,
                &agents,
                provider,
                this.runtime.tool_registry.clone(),
                &default_model,
                max_tool_rounds,
                eventbus,
                store,
                flow_name,
                flow_path,
            )
            .await?;
            Ok(dialog)
        });

        methods.add_async_method("run", |lua, this, ()| async move {
            let run_start = chrono::Utc::now();

            // Lazy agent-as-tool finalization — fails fast with a validation
            // error if any agent__<name> refs an unknown agent. Also handles
            // MCP augmentation internally (finalize_agent_tools layers agent
            // tools on top of the MCP-augmented registry), so this is a
            // drop-in replacement for the previous ad-hoc registry assembly.
            let finalized = this
                .ensure_agent_tools_finalized()
                .await
                .map_err(mlua::Error::external)?;
            let tool_registry = finalized.registry.clone();

            let mut crew = this.crew.lock().await;

            // If an EventBus was injected via Lua app_data (from API handler), use it.
            if let Some(eventbus) = lua.app_data_ref::<EventBus>() {
                crew.eventbus = eventbus.clone();
            }

            let provider: Arc<dyn LlmProvider> = match &this.custom_provider {
                Some(p) => p.clone(),
                None => this.runtime.provider.clone(),
            };
            let results = crew
                .run(provider, &tool_registry)
                .await
                .map_err(mlua::Error::external)?;

            let run_end = chrono::Utc::now();
            let total_ms = (run_end - run_start).num_milliseconds().max(0) as u64;

            // Save run history before returning so API callers can resolve this exact run.
            // If the API handler injected a run_id via app_data, use it for consistency.
            let pre_assigned_run_id: Option<String> =
                lua.app_data_ref::<String>().map(|r| r.clone());
            let mut record = crew.create_run_record(
                pre_assigned_run_id,
                &results,
                &run_start.to_rfc3339(),
                &run_end.to_rfc3339(),
                total_ms,
            );

            // Attach tags from CLI --tag flags or API input
            if let Some(tags) = lua.app_data_ref::<Vec<String>>() {
                record.tags = tags.clone();
            }
            let store = this
                .get_or_init_store()
                .await
                .map_err(mlua::Error::external)?;
            let run_id = store
                .save_run(&record)
                .await
                .map_err(mlua::Error::external)?;
            lua.globals().set("__ironcrew_last_run_id", run_id)?;

            // Convert results to Lua table
            let results_table = lua.create_table()?;
            for (i, result) in results.iter().enumerate() {
                let entry = lua.create_table()?;
                entry.set("task", result.task.clone())?;
                entry.set("agent", result.agent.clone())?;
                entry.set("output", result.output.clone())?;
                entry.set("success", result.success)?;
                entry.set("duration_ms", result.duration_ms)?;
                if let Some(ref usage) = result.token_usage {
                    let usage_table = lua.create_table()?;
                    usage_table.set("prompt_tokens", usage.prompt_tokens)?;
                    usage_table.set("completion_tokens", usage.completion_tokens)?;
                    usage_table.set("total_tokens", usage.total_tokens)?;
                    usage_table.set("cached_tokens", usage.cached_tokens)?;
                    entry.set("token_usage", usage_table)?;
                }
                results_table.set(i + 1, entry)?;
            }

            Ok(results_table)
        });
    }
}
