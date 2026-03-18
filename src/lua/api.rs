use std::path::{Path, PathBuf};
use std::sync::Arc;

use mlua::{Function, Lua, Result as LuaResult, Table, UserData, UserDataMethods, Value};
use tokio::sync::Mutex;

use crate::engine::agent::{Agent, ResponseFormat};
use crate::engine::crew::{Crew, ProviderConfig};
use crate::engine::runtime::Runtime;
use crate::engine::task::Task;
use crate::llm::openai::OpenAiProvider;
use crate::llm::provider::LlmProvider;
use crate::utils::error::{IronCrewError, Result};

// ---------------------------------------------------------------------------
// Agent parsing
// ---------------------------------------------------------------------------

/// Parse an Agent from a Lua table.
pub fn agent_from_lua_table(table: &Table) -> LuaResult<Agent> {
    let name: String = table.get("name")?;
    let goal: String = table.get("goal")?;
    let expected_output: Option<String> = table.get::<Option<String>>("expected_output")?.or(None);
    let system_prompt: Option<String> = table.get::<Option<String>>("system_prompt")?.or(None);
    let temperature: Option<f32> = table.get::<Option<f32>>("temperature")?.or(None);
    let max_tokens: Option<u32> = table.get::<Option<u32>>("max_tokens")?.or(None);
    let model: Option<String> = table.get::<Option<String>>("model")?.or(None);

    let capabilities: Vec<String> = table
        .get::<Table>("capabilities")
        .map(|t| {
            t.sequence_values::<String>()
                .filter_map(|v| v.ok())
                .collect()
        })
        .unwrap_or_default();

    let tools: Vec<String> = table
        .get::<Table>("tools")
        .map(|t| {
            t.sequence_values::<String>()
                .filter_map(|v| v.ok())
                .collect()
        })
        .unwrap_or_default();

    let response_format = parse_response_format(table)?;

    Ok(Agent {
        name,
        goal,
        expected_output,
        system_prompt,
        capabilities,
        tools,
        temperature,
        max_tokens,
        model,
        response_format,
    })
}

fn parse_response_format(table: &Table) -> LuaResult<Option<ResponseFormat>> {
    let rf_table: Table = match table.get("response_format") {
        Ok(t) => t,
        Err(_) => return Ok(None),
    };

    let rf_type: String = rf_table.get::<String>("type").unwrap_or_else(|_| "text".into());

    match rf_type.as_str() {
        "text" => Ok(Some(ResponseFormat::Text)),
        "json_object" => Ok(Some(ResponseFormat::JsonObject)),
        "json_schema" => {
            let name: String = rf_table.get("name").map_err(|_| {
                mlua::Error::external(IronCrewError::Validation(
                    "json_schema response_format requires 'name' field".into(),
                ))
            })?;
            let schema_table: Table = rf_table.get("schema").map_err(|_| {
                mlua::Error::external(IronCrewError::Validation(
                    "json_schema response_format requires 'schema' field".into(),
                ))
            })?;
            let schema = lua_table_to_json(&schema_table)?;
            Ok(Some(ResponseFormat::JsonSchema { name, schema }))
        }
        other => Err(mlua::Error::external(IronCrewError::Validation(
            format!("Unknown response_format type: '{}'", other),
        ))),
    }
}

// ---------------------------------------------------------------------------
// JSON conversion helpers
// ---------------------------------------------------------------------------

/// Recursively convert a Lua table to serde_json::Value.
pub fn lua_table_to_json(table: &Table) -> LuaResult<serde_json::Value> {
    // Check if it's an array (sequential integer keys starting at 1)
    let is_array = table.clone().sequence_values::<Value>().next().is_some()
        && table.clone().pairs::<Value, Value>().all(|pair| {
            pair.map(|(k, _)| matches!(k, Value::Integer(_)))
                .unwrap_or(false)
        });

    if is_array {
        let arr: Vec<serde_json::Value> = table
            .clone()
            .sequence_values::<Value>()
            .map(|v| lua_value_to_json(v.unwrap_or(Value::Nil)))
            .collect::<LuaResult<Vec<_>>>()?;
        Ok(serde_json::Value::Array(arr))
    } else {
        let mut map = serde_json::Map::new();
        for pair in table.clone().pairs::<String, Value>() {
            let (key, value) = pair?;
            map.insert(key, lua_value_to_json(value)?);
        }
        Ok(serde_json::Value::Object(map))
    }
}

pub fn lua_value_to_json(value: Value) -> LuaResult<serde_json::Value> {
    match value {
        Value::Nil => Ok(serde_json::Value::Null),
        Value::Boolean(b) => Ok(serde_json::Value::Bool(b)),
        Value::Integer(i) => Ok(serde_json::json!(i)),
        Value::Number(n) => Ok(serde_json::json!(n)),
        Value::String(s) => Ok(serde_json::Value::String(s.to_str()?.to_string())),
        Value::Table(t) => lua_table_to_json(&t),
        _ => Ok(serde_json::Value::Null),
    }
}

/// Convert a serde_json::Value into a Lua value.
pub fn json_value_to_lua(lua: &Lua, value: &serde_json::Value) -> LuaResult<Value> {
    match value {
        serde_json::Value::Null => Ok(Value::Nil),
        serde_json::Value::Bool(b) => Ok(Value::Boolean(*b)),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Ok(Value::Integer(i))
            } else if let Some(f) = n.as_f64() {
                Ok(Value::Number(f))
            } else {
                Ok(Value::Nil)
            }
        }
        serde_json::Value::String(s) => Ok(Value::String(lua.create_string(s)?)),
        serde_json::Value::Array(arr) => {
            let table = lua.create_table()?;
            for (i, v) in arr.iter().enumerate() {
                table.set(i + 1, json_value_to_lua(lua, v)?)?;
            }
            Ok(Value::Table(table))
        }
        serde_json::Value::Object(map) => {
            let table = lua.create_table()?;
            for (k, v) in map {
                table.set(k.as_str(), json_value_to_lua(lua, v)?)?;
            }
            Ok(Value::Table(table))
        }
    }
}

// ---------------------------------------------------------------------------
// Task parsing
// ---------------------------------------------------------------------------

/// Parse a Task from a Lua table.
pub fn task_from_lua_table(table: &Table) -> LuaResult<Task> {
    let name: String = table.get("name")?;
    let description: String = table.get("description")?;
    let agent: Option<String> = table.get::<Option<String>>("agent")?.or(None);
    let expected_output: Option<String> = table.get::<Option<String>>("expected_output")?.or(None);
    let context: Option<String> = table.get::<Option<String>>("context")?.or(None);

    let depends_on: Vec<String> = table
        .get::<Table>("depends_on")
        .map(|t| {
            t.sequence_values::<String>()
                .filter_map(|v| v.ok())
                .collect()
        })
        .unwrap_or_default();

    let max_retries: Option<u32> = table.get::<Option<u32>>("max_retries")?.or(None);
    let retry_backoff_secs: Option<f64> = table.get::<Option<f64>>("retry_backoff_secs")?.or(None);
    let timeout_secs: Option<u64> = table.get::<Option<u64>>("timeout_secs")?.or(None);
    let condition: Option<String> = table.get::<Option<String>>("condition")?.or(None);
    let on_error: Option<String> = table.get::<Option<String>>("on_error")?.or(None);

    Ok(Task {
        name,
        description,
        agent,
        expected_output,
        context,
        depends_on,
        max_retries,
        retry_backoff_secs,
        timeout_secs,
        condition,
        on_error,
    })
}

// ---------------------------------------------------------------------------
// Lua tool definitions
// ---------------------------------------------------------------------------

/// Metadata for a Lua-defined tool (parsed from tools/*.lua files).
pub struct LuaToolDef {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
    pub source_path: PathBuf,
}

/// Parse tool definition from a Lua table. Validates all required fields including execute.
pub fn tool_def_from_lua_table(table: &Table, source_path: &Path) -> LuaResult<LuaToolDef> {
    let name: String = table.get("name")?;
    let description: String = table.get("description")?;

    let params_table: Table = table.get("parameters")?;
    let parameters = lua_table_to_json(&params_table)?;

    // Validate execute function exists and is callable
    let _execute: Function = table.get("execute").map_err(|_| {
        mlua::Error::external(IronCrewError::Validation(format!(
            "Tool '{}' is missing required 'execute' function",
            name
        )))
    })?;

    // Convert our parameter format to JSON Schema
    let mut properties = serde_json::Map::new();
    let mut required = Vec::new();

    if let serde_json::Value::Object(params) = &parameters {
        for (key, value) in params {
            if let serde_json::Value::Object(param_def) = value {
                let mut prop = serde_json::Map::new();
                if let Some(t) = param_def.get("type") {
                    prop.insert("type".into(), t.clone());
                }
                if let Some(d) = param_def.get("description") {
                    prop.insert("description".into(), d.clone());
                }
                properties.insert(key.clone(), serde_json::Value::Object(prop));

                if param_def.get("required") == Some(&serde_json::Value::Bool(true)) {
                    required.push(serde_json::Value::String(key.clone()));
                }
            }
        }
    }

    let schema = serde_json::json!({
        "type": "object",
        "properties": properties,
        "required": required,
    });

    Ok(LuaToolDef {
        name,
        description,
        parameters: schema,
        source_path: source_path.to_path_buf(),
    })
}

/// Load tool definitions from Lua files (metadata only, not execute functions).
pub fn load_tool_defs_from_files(lua: &Lua, files: &[PathBuf]) -> Result<Vec<LuaToolDef>> {
    let mut tools = Vec::new();
    for file in files {
        let source = std::fs::read_to_string(file).map_err(|e| {
            IronCrewError::Validation(format!("Failed to read {}: {}", file.display(), e))
        })?;
        let table: Table = lua.load(&source).eval().map_err(IronCrewError::Lua)?;
        let tool_def = tool_def_from_lua_table(&table, file).map_err(|e| {
            IronCrewError::Validation(format!(
                "Invalid tool definition in {}: {}",
                file.display(),
                e
            ))
        })?;
        tracing::info!("Loaded tool '{}' from {}", tool_def.name, file.display());
        tools.push(tool_def);
    }
    Ok(tools)
}

// ---------------------------------------------------------------------------
// File-based agent loading
// ---------------------------------------------------------------------------

/// Load agent definitions from Lua files.
pub fn load_agents_from_files(lua: &Lua, files: &[PathBuf]) -> Result<Vec<Agent>> {
    let mut agents = Vec::new();
    for file in files {
        let source = std::fs::read_to_string(file).map_err(|e| {
            IronCrewError::Validation(format!("Failed to read {}: {}", file.display(), e))
        })?;
        let table: Table = lua.load(&source).eval().map_err(IronCrewError::Lua)?;
        let agent = agent_from_lua_table(&table).map_err(|e| {
            IronCrewError::Validation(format!(
                "Invalid agent definition in {}: {}",
                file.display(),
                e
            ))
        })?;
        tracing::info!("Loaded agent '{}' from {}", agent.name, file.display());
        agents.push(agent);
    }
    Ok(agents)
}

// ---------------------------------------------------------------------------
// Global registrations
// ---------------------------------------------------------------------------

/// Register the env() global function in Lua.
#[allow(dead_code)]
pub fn register_env_function(lua: &Lua) -> LuaResult<()> {
    let env_fn = lua.create_function(|_, name: String| Ok(std::env::var(&name).ok()))?;
    lua.globals().set("env", env_fn)?;
    Ok(())
}

/// Register Agent.new() constructor in Lua.
/// Validates the table and returns it back (so crew:add_agent() receives a table).
pub fn register_agent_constructor(lua: &Lua) -> LuaResult<()> {
    let agent_table = lua.create_table()?;

    let new_fn = lua.create_function(|_, table: Table| {
        // Validate the table has required fields
        agent_from_lua_table(&table)?;
        // Return the original table (not a serialized string)
        Ok(table)
    })?;

    agent_table.set("new", new_fn)?;
    lua.globals().set("Agent", agent_table)?;
    Ok(())
}

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
                // Resolve path relative to the project directory
                let project_dir = this.project_dir.clone();

                let flow_path = if Path::new(&path).is_absolute() {
                    PathBuf::from(&path)
                } else {
                    project_dir.join(&path)
                };

                if !flow_path.exists() {
                    return Err(mlua::Error::external(IronCrewError::Validation(
                        format!("Subworkflow not found: {}", flow_path.display()),
                    )));
                }

                // Parse options
                let output_key: Option<String> =
                    options.as_ref().and_then(|o| o.get("output_key").ok());
                let input_table: Option<Table> =
                    options.as_ref().and_then(|o| o.get("input").ok());

                // Serialize input to JSON so we can transfer between Lua states
                let input_json: Option<serde_json::Value> = match input_table {
                    Some(ref t) => Some(lua_table_to_json(t)?),
                    None => None,
                };

                // Load and execute the subworkflow
                let sub_lua =
                    crate::lua::sandbox::create_crew_lua().map_err(mlua::Error::external)?;

                // Register the same constructors
                register_agent_constructor(&sub_lua)?;

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
                let sub_agents = load_agents_from_files(&sub_lua, &sub_agent_files)
                    .map_err(mlua::Error::external)?;

                // Register Crew.new() for the sub-workflow with its own runtime
                let sub_project_dir = sub_dir.to_path_buf();
                register_crew_constructor(
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
                let script = std::fs::read_to_string(&flow_path).map_err(|e| {
                    mlua::Error::external(IronCrewError::Io(e))
                })?;

                let sub_result: Value = sub_lua.load(&script).eval_async().await?;

                // Return the result, optionally wrapped under output_key
                if let Some(key) = output_key {
                    let wrapper = lua.create_table()?;
                    // Transfer the value between Lua states by serializing through JSON
                    let json_str = match sub_result {
                        Value::Table(t) => {
                            let json = lua_table_to_json(&t)?;
                            serde_json::to_string(&json).unwrap_or_default()
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

        methods.add_async_method("run", |lua, this, ()| async move {
            let crew = this.crew.lock().await;
            let provider: Arc<dyn LlmProvider> = match &this.custom_provider {
                Some(p) => p.clone(),
                None => this.runtime.provider.clone(),
            };
            let results = crew
                .run(provider, &this.runtime.tool_registry)
                .await
                .map_err(mlua::Error::external)?;

            // Convert results to Lua table
            let results_table = lua.create_table()?;
            for (i, result) in results.iter().enumerate() {
                let entry = lua.create_table()?;
                entry.set("task", result.task.clone())?;
                entry.set("agent", result.agent.clone())?;
                entry.set("output", result.output.clone())?;
                entry.set("success", result.success)?;
                entry.set("duration_ms", result.duration_ms)?;
                results_table.set(i + 1, entry)?;
            }

            Ok(results_table)
        });
    }
}

/// Register Crew.new() constructor. Requires provider setup.
/// Preloaded agents (from agents/ directory) are auto-injected into every new Crew.
pub fn register_crew_constructor(
    lua: &Lua,
    runtime: Arc<Runtime>,
    preloaded_agents: Vec<Agent>,
    project_dir: PathBuf,
) -> LuaResult<()> {
    let crew_table = lua.create_table()?;
    let agents = Arc::new(preloaded_agents);
    let project_dir = Arc::new(project_dir);

    let new_fn = lua.create_function(move |_, table: Table| {
        let project_dir = (*project_dir).clone();
        let goal: String = table.get("goal")?;
        let provider: String =
            table.get::<String>("provider").unwrap_or_else(|_| "openai".into());
        let model: String =
            table.get::<String>("model").unwrap_or_else(|_| "gpt-4o-mini".into());
        let base_url: Option<String> = table.get("base_url").ok();
        let api_key: Option<String> = table.get("api_key").ok();
        let max_concurrent: Option<usize> = table.get::<Option<usize>>("max_concurrent").ok().flatten();

        // Create a custom provider if api_key or base_url differ from defaults
        let custom_provider: Option<Arc<dyn LlmProvider>> =
            if api_key.is_some() || base_url.is_some() {
                let key = api_key
                    .clone()
                    .or_else(|| std::env::var("OPENAI_API_KEY").ok())
                    .unwrap_or_default();
                let url = base_url.clone();
                Some(Arc::new(OpenAiProvider::new(key, url)))
            } else {
                None
            };

        let config = ProviderConfig {
            provider,
            model,
            base_url,
            api_key,
        };

        let mut crew = Crew::new(goal, config);
        crew.max_concurrent_tasks = max_concurrent;

        // Auto-inject preloaded agents from agents/ directory
        for agent in agents.iter() {
            crew.add_agent(agent.clone());
        }

        Ok(LuaCrew {
            crew: Arc::new(Mutex::new(crew)),
            runtime: runtime.clone(),
            custom_provider,
            project_dir,
        })
    })?;

    crew_table.set("new", new_fn)?;
    lua.globals().set("Crew", crew_table)?;
    Ok(())
}
