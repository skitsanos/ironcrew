use std::cell::RefCell;

use crate::lua::limits::{LuaExecutionGuard, LuaLimits};
use crate::lua::sandbox::{create_eval_lua, fresh_eval_environment};

// Thread-local Lua VM reused for hook execution to avoid per-call allocation.
// Hook bytecode is flow-supplied, so the VM is sandboxed like the crew VM and
// every call runs in a fresh environment (see `load_hook`).
thread_local! {
    static HOOK_LUA: RefCell<Option<std::result::Result<mlua::Lua, String>>> =
        const { RefCell::new(None) };
}

fn with_hook_lua<T>(fallback: T, operation: impl FnOnce(&mlua::Lua) -> T) -> T {
    HOOK_LUA.with(|cell| {
        let mut slot = cell.borrow_mut();
        let initialized = slot.get_or_insert_with(|| {
            let limits = LuaLimits::from_env().map_err(|error| error.to_string())?;
            create_eval_lua(limits).map_err(|error| error.to_string())
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

/// Load hook bytecode into a fresh per-call environment so one hook cannot
/// leave globals behind for the next hook on the same worker thread.
fn load_hook(
    lua: &mlua::Lua,
    bytecode: &[u8],
    kind: &str,
    task_name: &str,
) -> Option<mlua::Function> {
    let environment = match fresh_eval_environment(lua) {
        Ok(environment) => environment,
        Err(error) => {
            tracing::warn!(%error, kind, task_name, "hook environment could not be created");
            return None;
        }
    };
    match lua
        .load(bytecode)
        .set_environment(environment)
        .into_function()
    {
        Ok(function) => Some(function),
        Err(error) => {
            tracing::warn!(%error, kind, task_name, "hook failed to load");
            None
        }
    }
}

/// Run a before_task hook using the thread-local Lua VM.
/// Returns the (possibly modified) task description.
pub(super) fn run_before_hook(bytecode: &[u8], task_name: &str, task_description: &str) -> String {
    with_hook_lua(task_description.to_string(), |lua| {
        let Some(func) = load_hook(lua, bytecode, "before_task", task_name) else {
            return task_description.to_string();
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
        let Some(func) = load_hook(lua, bytecode, "after_task", task_name) else {
            return output.to_string();
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Compile hook source the way `add_agent` does: as a function dumped to
    /// bytecode from a separate VM.
    fn hook_bytecode(source: &str) -> Vec<u8> {
        let lua = mlua::Lua::new();
        lua.load(source)
            .into_function()
            .expect("hook source compiles")
            .dump(false)
    }

    fn run_before(source: &str) -> String {
        run_before_hook(&hook_bytecode(source), "task", "original")
    }

    #[test]
    fn hooks_cannot_reach_io_os_execute_or_package() {
        // A hook that resolves any of these would return the probe string.
        for probe in [
            "return io and 'reached' or 'blocked'",
            "return os.execute and 'reached' or 'blocked'",
            "return os.getenv and 'reached' or 'blocked'",
            "return os.exit and 'reached' or 'blocked'",
            "return os.remove and 'reached' or 'blocked'",
            "return package and 'reached' or 'blocked'",
            "return require and 'reached' or 'blocked'",
            "return loadfile and 'reached' or 'blocked'",
            "return dofile and 'reached' or 'blocked'",
        ] {
            assert_eq!(run_before(probe), "blocked", "hook reached {probe}");
        }
    }

    #[test]
    fn hooks_keep_permitted_os_helpers() {
        assert_eq!(
            run_before("return os.time and os.clock and 'kept' or 'missing'"),
            "kept"
        );
    }

    #[test]
    fn hook_globals_do_not_leak_into_later_hooks() {
        assert_eq!(run_before("smuggled = 'secret'; return 'first'"), "first");
        assert_eq!(
            run_before("return smuggled or 'absent'"),
            "absent",
            "a hook observed a global left by an earlier hook"
        );
    }

    #[test]
    fn after_hook_is_sandboxed_too() {
        let bytecode = hook_bytecode("return io and 'reached' or 'blocked'");
        assert_eq!(run_after_hook(&bytecode, "task", "output", true), "blocked");
    }

    #[test]
    fn failed_hook_load_returns_input_unchanged() {
        assert_eq!(
            run_before_hook(b"not valid bytecode", "task", "original"),
            "original"
        );
    }
}
