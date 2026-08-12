use async_trait::async_trait;
use mlua::Function;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, Weak};

use super::{Tool, ToolCallContext};
use crate::engine::conversation_definition::ConversationSourceContext;
use crate::engine::runtime::Runtime;
use crate::llm::provider::ToolSchema;
use crate::lua::limits::LuaExecutionGuard;
use crate::lua::sandbox::create_tool_lua_with_execution_policy;
use crate::lua::subflow::SubflowDepth;
use crate::tools::runtime_policy::LuaVmPolicy;
use crate::utils::error::{IronCrewError, Result};

pub struct LuaScriptTool {
    pub tool_name: String,
    pub description: String,
    pub parameters: serde_json::Value,
    pub source: Arc<str>,
    policy: super::execution_policy::LuaToolPolicy,
    vm_policy: LuaVmPolicy,
    conversation_source: Option<ConversationSourceContext>,
    /// Weak ref to the owning `Runtime`. Populated by `Runtime::set_self_ref`
    /// after the `Arc<Runtime>` is constructed so sub-flows can re-enter the
    /// same tool registry without a reference cycle.
    runtime: Mutex<Option<Weak<Runtime>>>,
    /// Project directory wrapped in `Arc` so sub-flow Lua VMs can pull it
    /// out of app-data without cloning a `PathBuf` per call.
    project_dir_arc: Mutex<Option<Arc<PathBuf>>>,
}

impl LuaScriptTool {
    pub(crate) fn new(
        tool_name: String,
        description: String,
        parameters: serde_json::Value,
        source: Arc<str>,
        fs_roots: (Option<PathBuf>, Option<PathBuf>),
        vm_policy: LuaVmPolicy,
        conversation_source: Option<ConversationSourceContext>,
    ) -> Self {
        let (read_base_dir, write_base_dir) = fs_roots;
        let policy = super::execution_policy::LuaToolPolicy::capture(
            read_base_dir.clone(),
            write_base_dir.clone(),
        );
        Self {
            tool_name,
            description,
            parameters,
            source,
            policy,
            vm_policy,
            conversation_source,
            runtime: Mutex::new(None),
            project_dir_arc: Mutex::new(None),
        }
    }

    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn with_fs_policy_for_test(
        tool_name: String,
        description: String,
        parameters: serde_json::Value,
        source: Arc<str>,
        read_base_dir: Option<PathBuf>,
        write_base_dir: Option<PathBuf>,
        read_limit: usize,
        write_limit: usize,
        http_marker: usize,
        allow_private: bool,
    ) -> Self {
        let policy = super::execution_policy::LuaToolPolicy::with_limits_for_test(
            read_base_dir,
            write_base_dir,
            read_limit,
            write_limit,
        );
        Self {
            tool_name,
            description,
            parameters,
            source,
            policy,
            vm_policy: LuaVmPolicy::for_test(http_marker, allow_private),
            conversation_source: None,
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

    fn conversation_definition(&self) -> Result<serde_json::Value> {
        Ok(serde_json::json!({
            "schema": self.schema(),
            "source_fingerprint": super::execution_policy::bytes_fingerprint(
                "lua-tool-source",
                self.source.as_bytes(),
            ),
            "policy": self.policy.definition()?,
            "vm_policy": self.vm_policy.definition(),
        }))
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolCallContext) -> Result<String> {
        let (read_root, write_root) =
            self.policy
                .roots()
                .map_err(|message| IronCrewError::ToolExecution {
                    tool: self.tool_name.clone(),
                    message,
                })?;
        let (read_limit, write_limit) =
            self.policy
                .limits()
                .map_err(|message| IronCrewError::ToolExecution {
                    tool: self.tool_name.clone(),
                    message,
                })?;
        let lua = create_tool_lua_with_execution_policy(
            read_root,
            write_root,
            read_limit,
            write_limit,
            self.vm_policy.clone(),
        )
        .map_err(IronCrewError::Lua)?;

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
        if let Some(context) = &self.conversation_source {
            lua.set_app_data(context.clone());
        }
        lua.set_app_data(SubflowDepth(ctx.depth));
        if let Some(ref eventbus) = ctx.eventbus {
            lua.set_app_data(eventbus.clone());
        }
        if let Some(ref store) = ctx.store {
            lua.set_app_data(store.clone());
        }

        // Loading the definition executes its top-level Lua, so the same
        // budget must cover both definition evaluation and the tool call.
        let _execution = LuaExecutionGuard::begin(&lua).map_err(IronCrewError::Lua)?;

        // Load the tool definition
        let table: mlua::Table = lua
            .load(self.source.as_ref())
            .eval()
            .map_err(IronCrewError::Lua)?;

        // Get the execute function
        let execute_fn: Function =
            table
                .get("execute")
                .map_err(|_| IronCrewError::ToolExecution {
                    tool: self.tool_name.clone(),
                    message: "Tool has no 'execute' function".into(),
                })?;

        // Convert JSON args to Lua table
        let json_limits = self.vm_policy.json_limits();
        let args_value = crate::lua::json::json_value_to_lua_with_limits(&lua, &args, json_limits)
            .map_err(IronCrewError::Lua)?;
        let args_table = match args_value {
            mlua::Value::Table(table) => table,
            other => {
                let table = lua.create_table().map_err(IronCrewError::Lua)?;
                table.set("value", other).map_err(IronCrewError::Lua)?;
                table
            }
        };

        // Call the function. Use `call_async` so any `run_flow` (or other
        // async primitives) nested inside the Lua execute block can await
        // cleanly instead of blocking the Tokio worker.
        let result: mlua::Value =
            execute_fn
                .call_async(args_table)
                .await
                .map_err(|e| IronCrewError::ToolExecution {
                    tool: self.tool_name.clone(),
                    message: format!("Lua execute error: {}", e),
                })?;

        let result = crate::lua::json::lua_value_to_json_with_limits(result, json_limits).map_err(
            |error| IronCrewError::ToolExecution {
                tool: self.tool_name.clone(),
                message: format!("Lua result conversion failed: {error}"),
            },
        )?;
        match result {
            serde_json::Value::String(string) => Ok(string),
            serde_json::Value::Null => Ok(String::new()),
            other => serde_json::to_string(&other).map_err(|error| IronCrewError::ToolExecution {
                tool: self.tool_name.clone(),
                message: format!("Lua result serialization failed: {error}"),
            }),
        }
    }
}
