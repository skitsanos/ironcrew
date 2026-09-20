use crate::lua::limits::{LuaExecutionGuard, LuaLimits};
use crate::lua::sandbox::{create_eval_lua, fresh_eval_environment};
use crate::metrics::{HookFailureStage, HookKind};

fn record_failure(kind: HookKind, stage: HookFailureStage) {
    crate::metrics::record_hook_failure(kind, stage);
}

fn with_hook_lua<T>(kind: HookKind, fallback: T, operation: impl FnOnce(&mlua::Lua) -> T) -> T {
    let lua = match LuaLimits::from_env()
        .map_err(|error| error.to_string())
        .and_then(|limits| create_eval_lua(limits).map_err(|error| error.to_string()))
    {
        Ok(lua) => lua,
        Err(error) => {
            tracing::error!(%error, "Hook Lua VM could not be initialized");
            record_failure(kind, HookFailureStage::VmInitialization);
            return fallback;
        }
    };
    let _execution = match LuaExecutionGuard::begin(&lua) {
        Ok(guard) => guard,
        Err(error) => {
            tracing::warn!(%error, "Hook Lua execution could not start");
            record_failure(kind, HookFailureStage::ExecutionStart);
            return fallback;
        }
    };
    operation(&lua)
}

/// Load hook bytecode into a fresh per-call environment so one hook cannot
/// leave globals behind for the next hook on the same worker thread.
fn load_hook(
    lua: &mlua::Lua,
    bytecode: &[u8],
    kind: HookKind,
    task_name: &str,
) -> Option<mlua::Function> {
    let environment = match fresh_eval_environment(lua) {
        Ok(environment) => environment,
        Err(error) => {
            tracing::warn!(%error, hook = kind.as_str(), task_name, "hook environment could not be created");
            record_failure(kind, HookFailureStage::Environment);
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
            tracing::warn!(%error, hook = kind.as_str(), task_name, "hook failed to load");
            record_failure(kind, HookFailureStage::Load);
            None
        }
    }
}

/// Run a before_task hook using the thread-local Lua VM.
/// Returns the (possibly modified) task description.
pub(super) fn run_before_hook(bytecode: &[u8], task_name: &str, task_description: &str) -> String {
    let kind = HookKind::BeforeTask;
    with_hook_lua(kind, task_description.to_string(), |lua| {
        let Some(func) = load_hook(lua, bytecode, kind, task_name) else {
            return task_description.to_string();
        };

        match func.call::<mlua::Value>((task_name, task_description)) {
            Ok(mlua::Value::String(s)) => match s.to_str() {
                Ok(s) => s.to_string(),
                Err(error) => {
                    tracing::warn!(%error, task_name, "before_task hook returned an invalid string");
                    record_failure(kind, HookFailureStage::ReturnValue);
                    task_description.to_string()
                }
            },
            Ok(mlua::Value::Nil) => task_description.to_string(),
            Ok(value) => {
                tracing::warn!(
                    task_name,
                    value_type = value.type_name(),
                    "before_task hook returned an unsupported value"
                );
                record_failure(kind, HookFailureStage::ReturnValue);
                task_description.to_string()
            }
            Err(error) => {
                tracing::warn!(%error, task_name, "before_task hook failed");
                record_failure(kind, HookFailureStage::Run);
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
    let kind = HookKind::AfterTask;
    with_hook_lua(kind, output.to_string(), |lua| {
        let Some(func) = load_hook(lua, bytecode, kind, task_name) else {
            return output.to_string();
        };

        match func.call::<mlua::Value>((task_name, output, success)) {
            Ok(mlua::Value::String(s)) => match s.to_str() {
                Ok(s) => s.to_string(),
                Err(error) => {
                    tracing::warn!(%error, task_name, "after_task hook returned an invalid string");
                    record_failure(kind, HookFailureStage::ReturnValue);
                    output.to_string()
                }
            },
            Ok(mlua::Value::Nil) => output.to_string(),
            Ok(value) => {
                tracing::warn!(
                    task_name,
                    value_type = value.type_name(),
                    "after_task hook returned an unsupported value"
                );
                record_failure(kind, HookFailureStage::ReturnValue);
                output.to_string()
            }
            Err(error) => {
                tracing::warn!(%error, task_name, "after_task hook failed");
                record_failure(kind, HookFailureStage::Run);
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
    fn hook_shared_global_escape_hatches_do_not_leak() {
        assert_eq!(
            run_before("_G.smuggled = 'secret'; os.smuggled = true; return 'first'"),
            "first"
        );
        assert_eq!(
            run_before("return (_G.smuggled or os.smuggled) and 'leaked' or 'absent'"),
            "absent"
        );
    }

    #[test]
    fn after_hook_is_sandboxed_too() {
        let bytecode = hook_bytecode("return io and 'reached' or 'blocked'");
        assert_eq!(run_after_hook(&bytecode, "task", "output", true), "blocked");
    }

    #[test]
    fn failed_hook_load_returns_input_unchanged() {
        let before = hook_failure_count(HookKind::BeforeTask, HookFailureStage::Load);
        assert_eq!(
            run_before_hook(b"not valid bytecode", "task", "original"),
            "original"
        );
        let after = hook_failure_count(HookKind::BeforeTask, HookFailureStage::Load);
        assert!(after >= before.saturating_add(1));
    }

    fn hook_failure_count(kind: HookKind, stage: HookFailureStage) -> u64 {
        let mut body = String::new();
        crate::metrics::append_prometheus(&mut body);
        let prefix = format!(
            "ironcrew_hook_failures_total{{hook=\"{}\",stage=\"{}\"}} ",
            kind.as_str(),
            stage.as_str()
        );
        body.lines()
            .find_map(|line| line.strip_prefix(&prefix))
            .expect("hook failure metric is exposed")
            .parse()
            .expect("hook failure metric is numeric")
    }
}
