use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use mlua::{Table, UserData, UserDataMethods, Value};
use tokio::sync::Mutex;

use crate::engine::crew::Crew;
use crate::engine::eventbus::EventBus;
use crate::engine::messagebus::{Message, MessageType};
use crate::engine::runtime::Runtime;
use crate::llm::provider::LlmProvider;
use crate::utils::error::IronCrewError;

use super::json::{json_value_to_lua, lua_table_to_json, lua_value_to_json};
use super::parsers::{agent_from_lua_table, load_agents_from_files, task_from_lua_table};

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
}

impl UserData for LuaCrew {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        // Use add_async_method for all methods to avoid block_on inside Tokio
        methods.add_async_method("add_agent", |_, this, table: Table| async move {
            let agent = agent_from_lua_table(&table)?;
            this.crew.lock().await.add_agent(agent);
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
                // Resolve path relative to the project directory.
                let flow_path = {
                    let flow_path = Path::new(&path);
                    if flow_path.as_os_str().is_empty()
                        || flow_path.is_absolute()
                        || flow_path.components().any(|c| {
                            matches!(
                                c,
                                Component::ParentDir
                                    | Component::RootDir
                                    | Component::Prefix(_)
                                    | Component::CurDir
                            )
                        })
                    {
                        return Err(mlua::Error::external(IronCrewError::Validation(
                            "Invalid subworkflow path".into(),
                        )));
                    }

                    let project_dir = this.project_dir.clone();
                    let candidate = project_dir.join(flow_path);
                    let base = project_dir
                        .canonicalize()
                        .unwrap_or_else(|_| project_dir.clone());
                    let canonical = candidate.canonicalize().map_err(|e| {
                        mlua::Error::external(IronCrewError::Validation(format!(
                            "Failed to resolve subworkflow '{}': {}",
                            path, e
                        )))
                    })?;

                    if !canonical.starts_with(&base) {
                        return Err(mlua::Error::external(IronCrewError::Validation(
                            "Subworkflow path escapes project directory".into(),
                        )));
                    }

                    canonical
                };

                if !flow_path.is_file() {
                    return Err(mlua::Error::external(IronCrewError::Validation(format!(
                        "Subworkflow not found: {}",
                        flow_path.display()
                    ))));
                }

                // Parse options
                let output_key: Option<String> =
                    options.as_ref().and_then(|o| o.get("output_key").ok());
                let input_table: Option<Table> = options.as_ref().and_then(|o| o.get("input").ok());

                // Serialize input to JSON so we can transfer between Lua states
                let input_json: Option<serde_json::Value> = match input_table {
                    Some(ref t) => Some(lua_table_to_json(t)?),
                    None => None,
                };

                // Load and execute the subworkflow
                let sub_lua =
                    crate::lua::sandbox::create_crew_lua().map_err(mlua::Error::external)?;

                // Register the same constructors
                super::api::register_agent_constructor(&sub_lua)?;

                // Load agents from sub-project if it's a directory
                let sub_dir = flow_path.parent().unwrap_or(Path::new("."));
                let sub_agents_dir = sub_dir.join("agents");
                let sub_agent_files = if sub_agents_dir.is_dir() {
                    std::fs::read_dir(&sub_agents_dir)
                        .into_iter()
                        .flatten()
                        .filter_map(|e| e.ok())
                        .map(|e| e.path())
                        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("lua"))
                        .collect::<Vec<_>>()
                } else {
                    vec![]
                };
                let sub_agents =
                    load_agents_from_files(&sub_agent_files).map_err(mlua::Error::external)?;

                // Register Crew.new() for the sub-workflow with its own runtime
                let sub_project_dir = sub_dir.to_path_buf();
                super::api::register_crew_constructor(
                    &sub_lua,
                    this.runtime.clone(),
                    sub_agents,
                    sub_project_dir,
                )?;

                // If input mapping provided, set it as globals
                if let Some(json) = &input_json {
                    let input_value = json_value_to_lua(&sub_lua, json)?;
                    sub_lua.globals().set("input", input_value)?;
                }

                // Execute the subworkflow script
                let script = std::fs::read_to_string(&flow_path)
                    .map_err(|e| mlua::Error::external(IronCrewError::Io(e)))?;

                let sub_result: Value = sub_lua.load(&script).eval_async().await?;

                // Return the result, optionally wrapped under output_key
                if let Some(key) = output_key {
                    let wrapper = lua.create_table()?;
                    // Transfer the value between Lua states by serializing through JSON
                    let json_str = match sub_result {
                        Value::Table(t) => {
                            let json = lua_table_to_json(&t)?;
                            serde_json::to_string(&json).map_err(|e| {
                                mlua::Error::external(IronCrewError::Validation(format!(
                                    "Failed to serialize subworkflow output: {}",
                                    e
                                )))
                            })?
                        }
                        Value::String(s) => s.to_str()?.to_string(),
                        _ => String::new(),
                    };
                    wrapper.set(key, json_str)?;
                    Ok(Value::Table(wrapper))
                } else {
                    // Transfer between Lua states via JSON serialization
                    match sub_result {
                        Value::Table(t) => {
                            let json = lua_table_to_json(&t)?;
                            let transferred = json_value_to_lua(&lua, &json)?;
                            Ok(transferred)
                        }
                        Value::String(s) => {
                            let s = s.to_str()?.to_string();
                            Ok(Value::String(lua.create_string(&s)?))
                        }
                        Value::Integer(i) => Ok(Value::Integer(i)),
                        Value::Number(n) => Ok(Value::Number(n)),
                        Value::Boolean(b) => Ok(Value::Boolean(b)),
                        Value::Nil => Ok(Value::Nil),
                        _ => Ok(Value::Nil),
                    }
                }
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

        methods.add_async_method("run", |lua, this, ()| async move {
            let run_start = chrono::Utc::now();

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
                .run(provider, &this.runtime.tool_registry)
                .await
                .map_err(mlua::Error::external)?;

            let run_end = chrono::Utc::now();
            let total_ms = (run_end - run_start).num_milliseconds().max(0) as u64;

            // Save run history before returning so API callers can resolve this exact run.
            let store_dir = this.project_dir.join(".ironcrew").join("runs");
            let record = crew.create_run_record(
                &results,
                &run_start.to_rfc3339(),
                &run_end.to_rfc3339(),
                total_ms,
            );
            let run_id =
                tokio::task::spawn_blocking(move || -> crate::utils::error::Result<String> {
                    let history = crate::engine::run_history::RunHistory::new(store_dir)?;
                    history.save(&record)
                })
                .await
                .map_err(|e| {
                    mlua::Error::external(IronCrewError::Task {
                        task: "run_history".into(),
                        message: format!("Failed to join run history task: {}", e),
                    })
                })?
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
