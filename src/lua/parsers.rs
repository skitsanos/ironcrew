use std::collections::BTreeMap;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use mlua::{Function, Result as LuaResult, Table, Value};

use crate::engine::agent::{Agent, ResponseFormat, validate_agent_tool_name};
use crate::engine::task::Task;
use crate::lua::limits::LuaExecutionGuard;
use crate::lua::sandbox::create_tool_lua;
use crate::utils::error::{IronCrewError, Result};

use super::json::lua_table_to_json;

const MAX_NAME_BYTES: usize = 128;
const MAX_PROVIDER_NAME_BYTES: usize = 64;
const MAX_TEXT_BYTES: usize = 256 * 1024;
const MAX_LIST_ITEMS: usize = 256;
const MAX_LIST_ITEM_BYTES: usize = 256;
const MAX_SCHEMA_BYTES: usize = 1024 * 1024;
const MAX_MODEL_BYTES: usize = 256;
const MAX_AGENT_TOKENS: u32 = 1_000_000;

fn validation_error(message: impl Into<String>) -> mlua::Error {
    mlua::Error::external(IronCrewError::Validation(message.into()))
}

fn validate_text(value: &str, field: &str) -> LuaResult<()> {
    if value.len() > MAX_TEXT_BYTES {
        return Err(validation_error(format!(
            "{field} exceeds the maximum length of {MAX_TEXT_BYTES} bytes"
        )));
    }
    Ok(())
}

fn validate_control_free(value: &str, field: &str, max_bytes: usize) -> LuaResult<()> {
    if value.trim().is_empty() {
        return Err(validation_error(format!("{field} must not be empty")));
    }
    if value.len() > max_bytes {
        return Err(validation_error(format!(
            "{field} exceeds the maximum length of {max_bytes} bytes"
        )));
    }
    if value.chars().any(char::is_control) {
        return Err(validation_error(format!(
            "{field} must not contain control characters"
        )));
    }
    Ok(())
}

fn validate_name(value: &str, field: &str) -> LuaResult<()> {
    validate_control_free(value, field, MAX_NAME_BYTES)
}

fn validate_provider_name(value: &str, field: &str) -> LuaResult<()> {
    validate_control_free(value, field, MAX_PROVIDER_NAME_BYTES)?;
    if !value
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return Err(validation_error(format!(
            "{field} must contain only ASCII letters, digits, '_' or '-'"
        )));
    }
    Ok(())
}

fn validate_optional_text(value: Option<&str>, field: &str) -> LuaResult<()> {
    if let Some(value) = value {
        validate_text(value, field)?;
    }
    Ok(())
}

fn validate_optional_control_free(
    value: Option<&str>,
    field: &str,
    max_bytes: usize,
) -> LuaResult<()> {
    if let Some(value) = value {
        validate_control_free(value, field, max_bytes)?;
    }
    Ok(())
}

fn parse_string_list(table: &Table, field: &str) -> LuaResult<Vec<String>> {
    let value: Value = table.raw_get(field)?;
    let Value::Table(list) = value else {
        if value == Value::Nil {
            return Ok(Vec::new());
        }
        return Err(validation_error(format!(
            "{field} must be a list of strings"
        )));
    };

    let mut indexed = BTreeMap::new();
    for pair in list.pairs::<Value, Value>() {
        let (key, value) = pair?;
        if indexed.len() >= MAX_LIST_ITEMS {
            return Err(validation_error(format!(
                "{field} exceeds the maximum of {MAX_LIST_ITEMS} items"
            )));
        }
        let Value::Integer(index) = key else {
            return Err(validation_error(format!(
                "{field} must use contiguous integer indexes starting at 1"
            )));
        };
        let index = usize::try_from(index).map_err(|_| {
            validation_error(format!(
                "{field} must use contiguous integer indexes starting at 1"
            ))
        })?;
        if index == 0 {
            return Err(validation_error(format!(
                "{field} must use contiguous integer indexes starting at 1"
            )));
        }
        let Value::String(value) = value else {
            return Err(validation_error(format!(
                "{field}[{index}] must be a string"
            )));
        };
        let value = value.to_str()?.to_string();
        validate_control_free(&value, &format!("{field}[{index}]"), MAX_LIST_ITEM_BYTES)?;
        indexed.insert(index, value);
    }

    for (expected, actual) in (1..=indexed.len()).zip(indexed.keys().copied()) {
        if expected != actual {
            return Err(validation_error(format!(
                "{field} must use contiguous integer indexes starting at 1"
            )));
        }
    }
    Ok(indexed.into_values().collect())
}

fn optional_bool(table: &Table, field: &str) -> LuaResult<bool> {
    match table.raw_get::<Value>(field)? {
        Value::Nil => Ok(false),
        Value::Boolean(value) => Ok(value),
        _ => Err(validation_error(format!("{field} must be a boolean"))),
    }
}

#[derive(Default)]
struct SizeWriter {
    bytes: usize,
    exceeded: bool,
}

impl Write for SizeWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.bytes.saturating_add(bytes.len()) > MAX_SCHEMA_BYTES {
            self.exceeded = true;
            return Err(io::Error::other("schema size limit exceeded"));
        }
        self.bytes += bytes.len();
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn validate_schema_size(schema: &serde_json::Value, field: &str) -> LuaResult<()> {
    let mut writer = SizeWriter::default();
    if let Err(error) = serde_json::to_writer(&mut writer, schema) {
        if writer.exceeded {
            return Err(validation_error(format!(
                "{field} exceeds the maximum serialized size of {MAX_SCHEMA_BYTES} bytes"
            )));
        }
        return Err(validation_error(format!(
            "failed to serialize {field}: {error}"
        )));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Agent parsing
// ---------------------------------------------------------------------------

/// Parse an Agent from a Lua table.
pub fn agent_from_lua_table(table: &Table) -> LuaResult<Agent> {
    let name: String = table.raw_get("name")?;
    let goal: String = table.raw_get("goal")?;
    let expected_output: Option<String> =
        table.raw_get::<Option<String>>("expected_output")?.or(None);
    let system_prompt: Option<String> = table.raw_get::<Option<String>>("system_prompt")?.or(None);
    let temperature: Option<f32> = table.raw_get::<Option<f32>>("temperature")?.or(None);
    let max_tokens: Option<u32> = table.raw_get::<Option<u32>>("max_tokens")?.or(None);
    let model: Option<String> = table.raw_get::<Option<String>>("model")?.or(None);

    validate_name(&name, "agent.name")?;
    validate_text(&goal, "agent.goal")?;
    validate_optional_text(expected_output.as_deref(), "agent.expected_output")?;
    validate_optional_text(system_prompt.as_deref(), "agent.system_prompt")?;
    validate_optional_control_free(model.as_deref(), "agent.model", MAX_MODEL_BYTES)?;
    if let Some(temperature) = temperature
        && (!temperature.is_finite() || !(0.0..=2.0).contains(&temperature))
    {
        return Err(validation_error(
            "agent.temperature must be finite and between 0 and 2",
        ));
    }
    if max_tokens.is_some_and(|tokens| tokens == 0 || tokens > MAX_AGENT_TOKENS) {
        return Err(validation_error(format!(
            "agent.max_tokens must be between 1 and {MAX_AGENT_TOKENS}"
        )));
    }

    let capabilities = parse_string_list(table, "capabilities")?;
    let tools = parse_string_list(table, "tools")?;

    // Validate any agent__<name> entries before we materialise the struct.
    for tool in &tools {
        validate_provider_name(tool, "agent.tools item")?;
        validate_agent_tool_name(tool).map_err(mlua::Error::external)?;
    }

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
    let rf_table = match table.raw_get::<Value>("response_format")? {
        Value::Nil => return Ok(None),
        Value::Table(table) => table,
        _ => {
            return Err(validation_error(
                "agent.response_format must be a table when set",
            ));
        }
    };

    let rf_type = match rf_table.raw_get::<Value>("type")? {
        Value::Nil => "text".to_string(),
        Value::String(value) => value.to_str()?.to_string(),
        _ => return Err(validation_error("response_format.type must be a string")),
    };

    match rf_type.as_str() {
        "text" => Ok(Some(ResponseFormat::Text)),
        "json_object" => Ok(Some(ResponseFormat::JsonObject)),
        "json_schema" => {
            let name: String = rf_table.raw_get("name").map_err(|_| {
                validation_error("json_schema response_format requires 'name' field")
            })?;
            validate_provider_name(&name, "response_format.name")?;
            let schema_table: Table = rf_table.raw_get("schema").map_err(|_| {
                validation_error("json_schema response_format requires 'schema' field")
            })?;
            let schema = lua_table_to_json(&schema_table)?;
            validate_schema_size(&schema, "response_format.schema")?;
            Ok(Some(ResponseFormat::JsonSchema { name, schema }))
        }
        other => Err(validation_error(format!(
            "Unknown response_format type: '{other}'"
        ))),
    }
}

// ---------------------------------------------------------------------------
// Task parsing
// ---------------------------------------------------------------------------

/// Parse a Task from a Lua table.
pub fn task_from_lua_table(table: &Table) -> LuaResult<Task> {
    let name: String = table.raw_get("name")?;
    let description: String = table.raw_get("description")?;
    let agent: Option<String> = table.raw_get::<Option<String>>("agent")?.or(None);
    let expected_output: Option<String> =
        table.raw_get::<Option<String>>("expected_output")?.or(None);
    let context: Option<String> = table.raw_get::<Option<String>>("context")?.or(None);

    validate_name(&name, "task.name")?;
    validate_text(&description, "task.description")?;
    validate_optional_text(expected_output.as_deref(), "task.expected_output")?;
    validate_optional_text(context.as_deref(), "task.context")?;
    if let Some(agent) = agent.as_deref() {
        validate_name(agent, "task.agent")?;
    }

    let depends_on = parse_string_list(table, "depends_on")?;
    for dependency in &depends_on {
        validate_name(dependency, "task.depends_on item")?;
    }

    let max_retries: Option<u32> = table.raw_get::<Option<u32>>("max_retries")?.or(None);
    let retry_backoff_secs: Option<f64> =
        table.raw_get::<Option<f64>>("retry_backoff_secs")?.or(None);
    let timeout_secs: Option<u64> = table.raw_get::<Option<u64>>("timeout_secs")?.or(None);
    let condition: Option<String> = table.raw_get::<Option<String>>("condition")?.or(None);
    let on_error: Option<String> = table.raw_get::<Option<String>>("on_error")?.or(None);
    let task_type: Option<String> = table.raw_get::<Option<String>>("task_type")?.or(None);
    let collaborative_agents = parse_string_list(table, "agents")?;
    let max_turns: Option<usize> = table.raw_get::<Option<usize>>("max_turns")?.or(None);
    let foreach_source: Option<String> = table.raw_get::<Option<String>>("foreach")?.or(None);
    let foreach_as: Option<String> = table.raw_get::<Option<String>>("foreach_as")?.or(None);
    let foreach_parallel = optional_bool(table, "foreach_parallel")?;
    let stream = optional_bool(table, "stream")?;
    let model: Option<String> = table.raw_get::<Option<String>>("model")?.or(None);

    for agent in &collaborative_agents {
        validate_name(agent, "task.agents item")?;
    }
    validate_optional_text(condition.as_deref(), "task.condition")?;
    if let Some(on_error) = on_error.as_deref() {
        validate_name(on_error, "task.on_error")?;
    }
    validate_optional_control_free(task_type.as_deref(), "task.task_type", MAX_LIST_ITEM_BYTES)?;
    validate_optional_control_free(
        foreach_source.as_deref(),
        "task.foreach",
        MAX_LIST_ITEM_BYTES,
    )?;
    validate_optional_control_free(
        foreach_as.as_deref(),
        "task.foreach_as",
        MAX_LIST_ITEM_BYTES,
    )?;
    validate_optional_control_free(model.as_deref(), "task.model", MAX_MODEL_BYTES)?;

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
        foreach_parallel,
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
    /// The same bytes that produced the validated metadata above. Runtime
    /// registration must never reopen `source_path` and execute different code.
    pub source: Arc<str>,
}

/// Parse tool definition from a Lua table. Validates all required fields including execute.
pub fn tool_def_from_lua_table(
    table: &Table,
    source_path: &Path,
    source: Arc<str>,
) -> LuaResult<LuaToolDef> {
    let name: String = table.raw_get("name")?;
    let description: String = table.raw_get("description")?;
    validate_provider_name(&name, "tool.name")?;
    validate_text(&description, "tool.description")?;

    let params_table: Table = table.raw_get("parameters")?;
    let parameters = lua_table_to_json(&params_table)?;
    validate_schema_size(&parameters, "tool.parameters")?;

    // Validate execute function exists and is callable
    let _execute: Function = table.raw_get("execute").map_err(|_| {
        mlua::Error::external(IronCrewError::Validation(format!(
            "Tool '{}' is missing required 'execute' function",
            name
        )))
    })?;

    // Convert our parameter format to JSON Schema
    let mut properties = serde_json::Map::new();
    let mut required = Vec::new();

    let serde_json::Value::Object(params) = &parameters else {
        return Err(validation_error("tool.parameters must be an object"));
    };
    for (key, value) in params {
        validate_control_free(key, "tool parameter name", MAX_LIST_ITEM_BYTES)?;
        let serde_json::Value::Object(param_def) = value else {
            return Err(validation_error(format!(
                "tool parameter '{key}' definition must be an object"
            )));
        };
        let mut prop = serde_json::Map::new();
        if let Some(value) = param_def.get("type") {
            let serde_json::Value::String(value) = value else {
                return Err(validation_error(format!(
                    "tool parameter '{key}' type must be a string"
                )));
            };
            validate_control_free(value, "tool parameter type", MAX_LIST_ITEM_BYTES)?;
            prop.insert("type".into(), serde_json::Value::String(value.clone()));
        }
        if let Some(value) = param_def.get("description") {
            let serde_json::Value::String(value) = value else {
                return Err(validation_error(format!(
                    "tool parameter '{key}' description must be a string"
                )));
            };
            validate_text(value, "tool parameter description")?;
            prop.insert(
                "description".into(),
                serde_json::Value::String(value.clone()),
            );
        }
        if let Some(required_value) = param_def.get("required")
            && !required_value.is_boolean()
        {
            return Err(validation_error(format!(
                "tool parameter '{key}' required flag must be a boolean"
            )));
        }
        properties.insert(key.clone(), serde_json::Value::Object(prop));

        if param_def.get("required") == Some(&serde_json::Value::Bool(true)) {
            required.push(serde_json::Value::String(key.clone()));
        }
    }

    let schema = serde_json::json!({
        "type": "object",
        "properties": properties,
        "required": required,
    });
    validate_schema_size(&schema, "tool schema")?;

    Ok(LuaToolDef {
        name,
        description,
        parameters: schema,
        source_path: source_path.to_path_buf(),
        source,
    })
}

/// Parse tool metadata while retaining the exact validated source for execution.
pub fn load_tool_defs_from_files(files: &[PathBuf]) -> Result<Vec<LuaToolDef>> {
    load_tool_defs_from_files_inner(files, false)
}

fn load_tool_defs_from_files_inner(
    files: &[PathBuf],
    http_conversation_bootstrap: bool,
) -> Result<Vec<LuaToolDef>> {
    let mut tools = Vec::new();
    for file in files {
        let source: Arc<str> = crate::lua::source::read_lua_source(file)
            .map(Arc::from)
            .map_err(|e| {
                IronCrewError::Validation(format!("Failed to read {}: {}", file.display(), e))
            })?;
        tools.push(parse_tool_source(
            file,
            source,
            http_conversation_bootstrap,
        )?);
    }
    Ok(tools)
}

pub(crate) fn load_tool_defs_from_snapshot(
    sources: &[crate::engine::conversation_definition::SnapshotLuaSource],
) -> Result<Vec<LuaToolDef>> {
    sources
        .iter()
        .map(|source| parse_tool_source(source.relative_path(), source.shared_source(), true))
        .collect()
}

fn parse_tool_source(
    file: &Path,
    source: Arc<str>,
    http_conversation_bootstrap: bool,
) -> Result<LuaToolDef> {
    let tool_lua = create_tool_lua().map_err(IronCrewError::Lua)?;
    if http_conversation_bootstrap {
        tool_lua.set_app_data(super::bootstrap::HttpConversationBootstrap);
    }
    let table: Table = {
        let _execution = LuaExecutionGuard::begin(&tool_lua).map_err(IronCrewError::Lua)?;
        tool_lua
            .load(source.as_ref())
            .into_function()
            .map_err(IronCrewError::Lua)?
            .call(())
            .map_err(IronCrewError::Lua)?
    };
    let tool_def = tool_def_from_lua_table(&table, file, source).map_err(|e| {
        IronCrewError::Validation(format!(
            "Invalid tool definition in {}: {}",
            file.display(),
            e
        ))
    })?;
    if tool_def.name.starts_with("agent__") {
        return Err(IronCrewError::Validation(format!(
            "Custom Lua tool at {} uses the reserved prefix 'agent__' \
                 (tool name '{}'). This prefix is reserved for agent-as-tool \
                 references.",
            file.display(),
            tool_def.name
        )));
    }
    tracing::info!("Loaded tool '{}' from {}", tool_def.name, file.display());
    Ok(tool_def)
}

// ---------------------------------------------------------------------------
// File-based agent loading
// ---------------------------------------------------------------------------

/// Load agent definitions from Lua files.
pub fn load_agents_from_files(files: &[PathBuf]) -> Result<Vec<Agent>> {
    load_agents_from_files_inner(files, false)
}

fn load_agents_from_files_inner(
    files: &[PathBuf],
    http_conversation_bootstrap: bool,
) -> Result<Vec<Agent>> {
    let mut agents = Vec::new();
    for file in files {
        let source: Arc<str> = crate::lua::source::read_lua_source(file)
            .map(Arc::from)
            .map_err(|e| {
                IronCrewError::Validation(format!("Failed to read {}: {}", file.display(), e))
            })?;
        agents.push(parse_agent_source(
            file,
            source,
            http_conversation_bootstrap,
        )?);
    }
    Ok(agents)
}

pub(crate) fn load_agents_from_snapshot(
    sources: &[crate::engine::conversation_definition::SnapshotLuaSource],
) -> Result<Vec<Agent>> {
    sources
        .iter()
        .map(|source| parse_agent_source(source.relative_path(), source.shared_source(), true))
        .collect()
}

fn parse_agent_source(
    file: &Path,
    source: Arc<str>,
    http_conversation_bootstrap: bool,
) -> Result<Agent> {
    let tool_lua = create_tool_lua().map_err(IronCrewError::Lua)?;
    if http_conversation_bootstrap {
        tool_lua.set_app_data(super::bootstrap::HttpConversationBootstrap);
    }
    let table: Table = {
        let _execution = LuaExecutionGuard::begin(&tool_lua).map_err(IronCrewError::Lua)?;
        tool_lua
            .load(source.as_ref())
            .into_function()
            .map_err(IronCrewError::Lua)?
            .call(())
            .map_err(IronCrewError::Lua)?
    };
    let agent = agent_from_lua_table(&table).map_err(|e| {
        IronCrewError::Validation(format!(
            "Invalid agent definition in {}: {}",
            file.display(),
            e
        ))
    })?;
    tracing::info!("Loaded agent '{}' from {}", agent.name, file.display());
    Ok(agent)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod parser_agent_tool_validation {
    use super::*;
    use mlua::Lua;
    use std::path::Path;

    fn base_agent(lua: &Lua) -> Table {
        let table = lua.create_table().unwrap();
        table.set("name", "coordinator").unwrap();
        table.set("goal", "route asks").unwrap();
        table
    }

    fn base_task(lua: &Lua) -> Table {
        let table = lua.create_table().unwrap();
        table.set("name", "research").unwrap();
        table.set("description", "Research the topic").unwrap();
        table
    }

    #[test]
    fn agent_from_lua_table_rejects_malformed_agent_tool_name() {
        let lua = Lua::new();
        let table = base_agent(&lua);
        let tools = lua.create_sequence_from(vec!["agent__BadCase"]).unwrap();
        table.set("tools", tools).unwrap();

        let result = agent_from_lua_table(&table);
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("agent__BadCase"),
            "error should mention the malformed name, got: {err}"
        );
    }

    #[test]
    fn agent_from_lua_table_accepts_valid_agent_tool_name() {
        let lua = Lua::new();
        let table = base_agent(&lua);
        let tools = lua.create_sequence_from(vec!["agent__researcher"]).unwrap();
        table.set("tools", tools).unwrap();

        let result = agent_from_lua_table(&table);
        assert!(
            result.is_ok(),
            "valid agent tool name rejected: {:?}",
            result.err()
        );
    }

    #[test]
    fn agent_rejects_invalid_scalar_limits() {
        let lua = Lua::new();

        let table = base_agent(&lua);
        table.set("name", "bad\nname").unwrap();
        assert!(
            agent_from_lua_table(&table)
                .unwrap_err()
                .to_string()
                .contains("control")
        );

        let table = base_agent(&lua);
        table.set("temperature", f64::NAN).unwrap();
        assert!(
            agent_from_lua_table(&table)
                .unwrap_err()
                .to_string()
                .contains("temperature")
        );

        let table = base_agent(&lua);
        table.set("max_tokens", 0).unwrap();
        assert!(
            agent_from_lua_table(&table)
                .unwrap_err()
                .to_string()
                .contains("max_tokens")
        );

        let table = base_agent(&lua);
        table
            .set("system_prompt", "x".repeat(MAX_TEXT_BYTES + 1))
            .unwrap();
        assert!(
            agent_from_lua_table(&table)
                .unwrap_err()
                .to_string()
                .contains("system_prompt")
        );
    }

    #[test]
    fn agent_lists_are_strict_contiguous_and_bounded() {
        let lua = Lua::new();

        let table = base_agent(&lua);
        let tools = lua.create_table().unwrap();
        tools.set(1, "file_read").unwrap();
        tools.set(2, true).unwrap();
        table.set("tools", tools).unwrap();
        assert!(
            agent_from_lua_table(&table)
                .unwrap_err()
                .to_string()
                .contains("must be a string")
        );

        let table = base_agent(&lua);
        let capabilities = lua.create_table().unwrap();
        capabilities.set(2, "analysis").unwrap();
        table.set("capabilities", capabilities).unwrap();
        assert!(
            agent_from_lua_table(&table)
                .unwrap_err()
                .to_string()
                .contains("contiguous")
        );

        let table = base_agent(&lua);
        let capabilities = lua.create_table().unwrap();
        for index in 1..=MAX_LIST_ITEMS + 1 {
            capabilities.set(index, "analysis").unwrap();
        }
        table.set("capabilities", capabilities).unwrap();
        assert!(
            agent_from_lua_table(&table)
                .unwrap_err()
                .to_string()
                .contains("maximum")
        );

        let table = base_agent(&lua);
        let tools = lua.create_sequence_from(["not.provider.safe"]).unwrap();
        table.set("tools", tools).unwrap();
        assert!(
            agent_from_lua_table(&table)
                .unwrap_err()
                .to_string()
                .contains("ASCII")
        );
    }

    #[test]
    fn response_schema_name_and_size_are_bounded() {
        let lua = Lua::new();

        let table = base_agent(&lua);
        let format = lua.create_table().unwrap();
        format.set("type", "json_schema").unwrap();
        format.set("name", "invalid.schema").unwrap();
        format.set("schema", lua.create_table().unwrap()).unwrap();
        table.set("response_format", format).unwrap();
        assert!(
            agent_from_lua_table(&table)
                .unwrap_err()
                .to_string()
                .contains("ASCII")
        );

        let table = base_agent(&lua);
        let format = lua.create_table().unwrap();
        format.set("type", "json_schema").unwrap();
        format.set("name", "valid_schema").unwrap();
        let schema = lua.create_table().unwrap();
        schema
            .set("description", "x".repeat(MAX_SCHEMA_BYTES + 1))
            .unwrap();
        format.set("schema", schema).unwrap();
        table.set("response_format", format).unwrap();
        assert!(
            agent_from_lua_table(&table)
                .unwrap_err()
                .to_string()
                .contains("serialized size")
        );
    }

    #[test]
    fn task_rejects_bad_names_lists_and_boolean_fields() {
        let lua = Lua::new();

        let table = base_task(&lua);
        table.set("name", "").unwrap();
        assert!(
            task_from_lua_table(&table)
                .unwrap_err()
                .to_string()
                .contains("must not be empty")
        );

        let table = base_task(&lua);
        let dependencies = lua.create_table().unwrap();
        dependencies.set(1, "setup").unwrap();
        dependencies.set(3, "missing-middle").unwrap();
        table.set("depends_on", dependencies).unwrap();
        assert!(
            task_from_lua_table(&table)
                .unwrap_err()
                .to_string()
                .contains("contiguous")
        );

        let table = base_task(&lua);
        table.set("stream", "yes").unwrap();
        assert!(
            task_from_lua_table(&table)
                .unwrap_err()
                .to_string()
                .contains("boolean")
        );
    }

    #[test]
    fn custom_tool_name_and_parameter_definitions_are_strict() {
        let lua = Lua::new();
        let table = lua.create_table().unwrap();
        table.set("name", "bad.tool").unwrap();
        table.set("description", "description").unwrap();
        table
            .set("parameters", lua.create_table().unwrap())
            .unwrap();
        table
            .set("execute", lua.create_function(|_, ()| Ok(())).unwrap())
            .unwrap();
        assert!(
            tool_def_from_lua_table(&table, Path::new("tool.lua"), Arc::from("return {}"))
                .err()
                .unwrap()
                .to_string()
                .contains("ASCII")
        );

        table.set("name", "valid_tool").unwrap();
        let parameters = lua.create_table().unwrap();
        parameters.set("query", "not a table").unwrap();
        table.set("parameters", parameters).unwrap();
        assert!(
            tool_def_from_lua_table(&table, Path::new("tool.lua"), Arc::from("return {}"))
                .err()
                .unwrap()
                .to_string()
                .contains("definition must be an object")
        );
    }
}
