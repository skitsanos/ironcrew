use std::path::{Path, PathBuf};

use mlua::{Function, Result as LuaResult, Table};

use crate::engine::agent::{Agent, ResponseFormat};
use crate::engine::task::Task;
use crate::lua::sandbox::create_tool_lua;
use crate::utils::error::{IronCrewError, Result};

use super::json::lua_table_to_json;

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

    let rf_type: String = rf_table
        .get::<String>("type")
        .unwrap_or_else(|_| "text".into());

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
        other => Err(mlua::Error::external(IronCrewError::Validation(format!(
            "Unknown response_format type: '{}'",
            other
        )))),
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
    let task_type: Option<String> = table.get::<Option<String>>("task_type")?.or(None);
    let collaborative_agents: Vec<String> = table
        .get::<Table>("agents")
        .map(|t| {
            t.sequence_values::<String>()
                .filter_map(|v| v.ok())
                .collect()
        })
        .unwrap_or_default();
    let max_turns: Option<usize> = table.get::<Option<usize>>("max_turns")?.or(None);
    let foreach_source: Option<String> = table.get::<Option<String>>("foreach")?.or(None);
    let foreach_as: Option<String> = table.get::<Option<String>>("foreach_as")?.or(None);
    let stream: bool = table.get::<bool>("stream").unwrap_or(false);
    let model: Option<String> = table.get::<Option<String>>("model")?.or(None);

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
        task_type,
        collaborative_agents,
        max_turns,
        foreach_source,
        foreach_as,
        stream,
        model,
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
pub fn load_tool_defs_from_files(files: &[PathBuf]) -> Result<Vec<LuaToolDef>> {
    let mut tools = Vec::new();
    for file in files {
        let source = std::fs::read_to_string(file).map_err(|e| {
            IronCrewError::Validation(format!("Failed to read {}: {}", file.display(), e))
        })?;
        let tool_lua = create_tool_lua().map_err(IronCrewError::Lua)?;
        let table: Table = tool_lua
            .load(&source)
            .into_function()
            .map_err(IronCrewError::Lua)?
            .call(())
            .map_err(IronCrewError::Lua)?;
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
pub fn load_agents_from_files(files: &[PathBuf]) -> Result<Vec<Agent>> {
    let mut agents = Vec::new();
    for file in files {
        let source = std::fs::read_to_string(file).map_err(|e| {
            IronCrewError::Validation(format!("Failed to read {}: {}", file.display(), e))
        })?;
        let tool_lua = create_tool_lua().map_err(IronCrewError::Lua)?;
        let table: Table = tool_lua
            .load(&source)
            .into_function()
            .map_err(IronCrewError::Lua)?
            .call(())
            .map_err(IronCrewError::Lua)?;
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
