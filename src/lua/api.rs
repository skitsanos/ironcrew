use mlua::{Lua, Result as LuaResult, Table, Value};

use crate::engine::agent::{Agent, ResponseFormat};
use crate::engine::task::Task;
use crate::utils::error::{IronCrewError, Result};

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

/// Recursively convert a Lua table to serde_json::Value.
pub fn lua_table_to_json(table: &Table) -> LuaResult<serde_json::Value> {
    // Check if it's an array (sequential integer keys starting at 1)
    let is_array = table.clone().sequence_values::<Value>().next().is_some()
        && table.clone().pairs::<Value, Value>().all(|pair| {
            pair.map(|(k, _)| matches!(k, Value::Integer(_))).unwrap_or(false)
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

fn lua_value_to_json(value: Value) -> LuaResult<serde_json::Value> {
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

    Ok(Task {
        name,
        description,
        agent,
        expected_output,
        context,
        depends_on,
    })
}

/// Load agent definitions from Lua files.
pub fn load_agents_from_files(lua: &Lua, files: &[std::path::PathBuf]) -> Result<Vec<Agent>> {
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

/// Register the env() global function in Lua.
pub fn register_env_function(lua: &Lua) -> LuaResult<()> {
    let env_fn = lua.create_function(|_, name: String| {
        Ok(std::env::var(&name).ok())
    })?;
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
