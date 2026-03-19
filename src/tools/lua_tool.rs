use async_trait::async_trait;
use mlua::{Function, Value};
use std::path::PathBuf;

use super::Tool;
use crate::llm::provider::ToolSchema;
use crate::lua::sandbox::create_tool_lua_with_base_dir;
use crate::utils::error::{IronCrewError, Result};

pub struct LuaScriptTool {
    pub tool_name: String,
    pub description: String,
    pub parameters: serde_json::Value,
    pub source: String,
    pub base_dir: Option<PathBuf>,
}

impl LuaScriptTool {
    pub fn new(
        tool_name: String,
        description: String,
        parameters: serde_json::Value,
        source: String,
        base_dir: Option<PathBuf>,
    ) -> Self {
        Self {
            tool_name,
            description,
            parameters,
            source,
            base_dir,
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
        let lua =
            create_tool_lua_with_base_dir(self.base_dir.clone()).map_err(IronCrewError::Lua)?;

        // Load the tool definition
        let table: mlua::Table = lua.load(&self.source).eval().map_err(IronCrewError::Lua)?;

        // Get the execute function
        let execute_fn: Function =
            table
                .get("execute")
                .map_err(|_| IronCrewError::ToolExecution {
                    tool: self.tool_name.clone(),
                    message: "Tool has no 'execute' function".into(),
                })?;

        // Convert JSON args to Lua table
        let args_value = json_to_lua_value(&lua, &args).map_err(IronCrewError::Lua)?;
        let args_table = match args_value {
            Value::Table(table) => table,
            other => {
                let table = lua.create_table().map_err(IronCrewError::Lua)?;
                table.set("value", other).map_err(IronCrewError::Lua)?;
                table
            }
        };

        // Call the function
        let result: Value =
            execute_fn
                .call(args_table)
                .map_err(|e| IronCrewError::ToolExecution {
                    tool: self.tool_name.clone(),
                    message: format!("Lua execute error: {}", e),
                })?;

        match result {
            Value::String(s) => Ok(s.to_str()?.to_string()),
            Value::Nil => Ok(String::new()),
            other => Ok(format!("{:?}", other)),
        }
    }
}

fn json_to_lua_value(lua: &mlua::Lua, value: &serde_json::Value) -> mlua::Result<mlua::Value> {
    match value {
        serde_json::Value::Null => Ok(mlua::Value::Nil),
        serde_json::Value::Bool(boolean) => Ok(mlua::Value::Boolean(*boolean)),
        serde_json::Value::Number(number) => {
            if let Some(integer) = number.as_i64() {
                Ok(mlua::Value::Integer(integer))
            } else if let Some(float) = number.as_f64() {
                Ok(mlua::Value::Number(float))
            } else {
                Ok(mlua::Value::Nil)
            }
        }
        serde_json::Value::String(string) => {
            Ok(mlua::Value::String(lua.create_string(string.as_str())?))
        }
        serde_json::Value::Array(array) => {
            let table = lua.create_table()?;
            for (index, item) in array.iter().enumerate() {
                table.set(index + 1, json_to_lua_value(lua, item)?)?;
            }
            Ok(mlua::Value::Table(table))
        }
        serde_json::Value::Object(map) => {
            let table = lua.create_table()?;
            for (key, item) in map {
                table.set(key.as_str(), json_to_lua_value(lua, item)?)?;
            }
            Ok(mlua::Value::Table(table))
        }
    }
}
