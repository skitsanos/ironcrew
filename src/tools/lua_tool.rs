use async_trait::async_trait;
use mlua::{Function, Value};

use super::Tool;
use crate::llm::provider::ToolSchema;
use crate::lua::sandbox::create_tool_lua;
use crate::utils::error::{IronCrewError, Result};

pub struct LuaScriptTool {
    pub tool_name: String,
    pub description: String,
    pub parameters: serde_json::Value,
    pub source: String,
}

impl LuaScriptTool {
    pub fn new(
        tool_name: String,
        description: String,
        parameters: serde_json::Value,
        source: String,
    ) -> Self {
        Self {
            tool_name,
            description,
            parameters,
            source,
        }
    }
}

#[async_trait]
impl Tool for LuaScriptTool {
    fn name(&self) -> &str {
        &self.tool_name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: self.tool_name.clone(),
            description: self.description.clone(),
            parameters: self.parameters.clone(),
        }
    }

    async fn execute(&self, args: serde_json::Value) -> Result<String> {
        let lua = create_tool_lua().map_err(IronCrewError::Lua)?;

        // Load the tool definition
        let table: mlua::Table = lua.load(&self.source).eval().map_err(IronCrewError::Lua)?;

        // Get the execute function
        let execute_fn: Function = table.get("execute").map_err(|_| {
            IronCrewError::ToolExecution {
                tool: self.tool_name.clone(),
                message: "Tool has no 'execute' function".into(),
            }
        })?;

        // Convert JSON args to Lua table
        let args_table = json_to_lua_table(&lua, &args).map_err(IronCrewError::Lua)?;

        // Call the function
        let result: Value = execute_fn.call(args_table).map_err(|e| {
            IronCrewError::ToolExecution {
                tool: self.tool_name.clone(),
                message: format!("Lua execute error: {}", e),
            }
        })?;

        match result {
            Value::String(s) => Ok(s.to_str()?.to_string()),
            Value::Nil => Ok(String::new()),
            other => Ok(format!("{:?}", other)),
        }
    }
}

fn json_to_lua_table(lua: &mlua::Lua, value: &serde_json::Value) -> mlua::Result<mlua::Table> {
    let table = lua.create_table()?;
    if let serde_json::Value::Object(map) = value {
        for (key, val) in map {
            match val {
                serde_json::Value::String(s) => table.set(key.as_str(), s.as_str())?,
                serde_json::Value::Number(n) => {
                    if let Some(i) = n.as_i64() {
                        table.set(key.as_str(), i)?;
                    } else if let Some(f) = n.as_f64() {
                        table.set(key.as_str(), f)?;
                    }
                }
                serde_json::Value::Bool(b) => table.set(key.as_str(), *b)?,
                serde_json::Value::Null => table.set(key.as_str(), mlua::Value::Nil)?,
                _ => {}
            }
        }
    }
    Ok(table)
}
