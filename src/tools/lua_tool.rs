use async_trait::async_trait;
use mlua::{Function, Value};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, Weak};

use super::{Tool, ToolCallContext};
use crate::engine::runtime::Runtime;
use crate::llm::provider::ToolSchema;
use crate::lua::sandbox::create_tool_lua_with_base_dir;
use crate::lua::subflow::SubflowDepth;
use crate::utils::error::{IronCrewError, Result};

pub struct LuaScriptTool {
    pub tool_name: String,
    pub description: String,
    pub parameters: serde_json::Value,
    pub source: String,
    pub base_dir: Option<PathBuf>,
    /// Weak ref to the owning `Runtime`. Populated by `Runtime::set_self_ref`
    /// after the `Arc<Runtime>` is constructed so sub-flows can re-enter the
    /// same tool registry without a reference cycle.
    runtime: Mutex<Option<Weak<Runtime>>>,
    /// Project directory wrapped in `Arc` so sub-flow Lua VMs can pull it
    /// out of app-data without cloning a `PathBuf` per call.
    project_dir_arc: Mutex<Option<Arc<PathBuf>>>,
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
            runtime: Mutex::new(None),
            project_dir_arc: Mutex::new(None),
        }
    }

    /// Populate the weak `Runtime` reference. Called from
    /// `Runtime::set_self_ref` once the owning `Arc<Runtime>` exists.
    pub fn set_runtime(&self, runtime: Weak<Runtime>) {
        if let Ok(mut guard) = self.runtime.lock() {
            *guard = Some(runtime);
        }
    }

    /// Populate the shared project-directory `Arc`. Called alongside
    /// `set_runtime` so both pieces of state arrive together.
    pub fn set_project_dir(&self, project_dir: Arc<PathBuf>) {
        if let Ok(mut guard) = self.project_dir_arc.lock() {
            *guard = Some(project_dir);
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

    async fn execute(&self, args: serde_json::Value, ctx: &ToolCallContext) -> Result<String> {
        let lua =
            create_tool_lua_with_base_dir(self.base_dir.clone()).map_err(IronCrewError::Lua)?;

        // Seed app-data on the sandbox VM so sandbox-level primitives (like
        // `run_flow`) can reach the runtime + project dir + current subflow
        // depth. Missing values silently turn the primitive into a clean
        // error at fire-time — registration still succeeds.
        if let Ok(guard) = self.runtime.lock()
            && let Some(ref weak) = *guard
            && let Some(runtime) = weak.upgrade()
        {
            lua.set_app_data(runtime);
        }
        if let Ok(guard) = self.project_dir_arc.lock()
            && let Some(ref project_dir) = *guard
        {
            lua.set_app_data(project_dir.clone());
        }
        lua.set_app_data(SubflowDepth(ctx.depth));
        if let Some(ref eventbus) = ctx.eventbus {
            lua.set_app_data(eventbus.clone());
        }
        if let Some(ref store) = ctx.store {
            lua.set_app_data(store.clone());
        }

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

        // Call the function. Use `call_async` so any `run_flow` (or other
        // async primitives) nested inside the Lua execute block can await
        // cleanly instead of blocking the Tokio worker.
        let result: Value =
            execute_fn
                .call_async(args_table)
                .await
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
