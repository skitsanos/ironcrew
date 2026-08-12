use std::cell::RefCell;

use crate::lua::limits::{LuaExecutionGuard, LuaLimits, install_lua_limits};

// Thread-local Lua VM reused for hook execution to avoid per-call allocation.
thread_local! {
    static HOOK_LUA: RefCell<Option<std::result::Result<mlua::Lua, String>>> =
        const { RefCell::new(None) };
}

fn with_hook_lua<T>(fallback: T, operation: impl FnOnce(&mlua::Lua) -> T) -> T {
    HOOK_LUA.with(|cell| {
        let mut slot = cell.borrow_mut();
        let initialized = slot.get_or_insert_with(|| {
            let lua = mlua::Lua::new();
            install_lua_limits(
                &lua,
                LuaLimits::from_env().map_err(|error| error.to_string())?,
            )
            .map_err(|error| error.to_string())?;
            Ok(lua)
        });
        let lua = match initialized {
            Ok(lua) => lua,
            Err(error) => {
                tracing::error!(%error, "Hook Lua VM could not be initialized");
                return fallback;
            }
        };
        let _execution = match LuaExecutionGuard::begin(lua) {
            Ok(guard) => guard,
            Err(error) => {
                tracing::warn!(%error, "Hook Lua execution could not start");
                return fallback;
            }
        };
        operation(lua)
    })
}

/// Run a before_task hook using the thread-local Lua VM.
/// Returns the (possibly modified) task description.
pub(super) fn run_before_hook(bytecode: &[u8], task_name: &str, task_description: &str) -> String {
    with_hook_lua(task_description.to_string(), |lua| {
        let func = match lua.load(bytecode).into_function() {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!(
                    "before_task hook for task '{}' failed to load: {}",
                    task_name,
                    e
                );
                return task_description.to_string();
            }
        };

        match func.call::<mlua::Value>((task_name, task_description)) {
            Ok(mlua::Value::String(s)) => match s.to_str() {
                Ok(s) => s.to_string(),
                Err(_) => task_description.to_string(),
            },
            Ok(mlua::Value::Nil) => task_description.to_string(),
            Ok(_) => task_description.to_string(),
            Err(e) => {
                tracing::warn!("before_task hook for task '{}' failed: {}", task_name, e);
                task_description.to_string()
            }
        }
    })
}

/// Run an after_task hook using the thread-local Lua VM.
/// Returns the (possibly modified) output.
pub(super) fn run_after_hook(
    bytecode: &[u8],
    task_name: &str,
    output: &str,
    success: bool,
) -> String {
    with_hook_lua(output.to_string(), |lua| {
        let func = match lua.load(bytecode).into_function() {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!(
                    "after_task hook for task '{}' failed to load: {}",
                    task_name,
                    e
                );
                return output.to_string();
            }
        };

        match func.call::<mlua::Value>((task_name, output, success)) {
            Ok(mlua::Value::String(s)) => match s.to_str() {
                Ok(s) => s.to_string(),
                Err(_) => output.to_string(),
            },
            Ok(mlua::Value::Nil) => output.to_string(),
            Ok(_) => output.to_string(),
            Err(e) => {
                tracing::warn!("after_task hook for task '{}' failed: {}", task_name, e);
                output.to_string()
            }
        }
    })
}
