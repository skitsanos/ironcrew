use std::cell::RefCell;
use std::collections::HashMap;

use crate::engine::task::TaskResult;
use crate::lua::json::json_value_to_lua;
use crate::lua::limits::{LuaExecutionGuard, LuaLimits, install_lua_limits};

// Thread-local Lua VM reused for condition evaluation.
thread_local! {
    static CONDITION_LUA: RefCell<Option<std::result::Result<mlua::Lua, String>>> =
        const { RefCell::new(None) };
}

pub fn evaluate_condition(condition: &str, results: &HashMap<String, TaskResult>) -> bool {
    CONDITION_LUA.with(|cell| {
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
        match initialized {
            Ok(lua) => evaluate_condition_inner(lua, condition, results),
            Err(error) => {
                tracing::error!(%error, "Condition Lua VM could not be initialized");
                false
            }
        }
    })
}

fn evaluate_condition_inner(
    lua: &mlua::Lua,
    condition: &str,
    results: &HashMap<String, TaskResult>,
) -> bool {
    let _execution = match LuaExecutionGuard::begin(lua) {
        Ok(guard) => guard,
        Err(error) => {
            tracing::warn!(%error, "Condition Lua execution could not start");
            return false;
        }
    };
    let Ok(ctx) = lua.create_table() else {
        return false;
    };
    for (name, result) in results {
        let Ok(entry) = lua.create_table() else {
            continue;
        };
        let _ = entry.set("output", result.output.clone());
        let _ = entry.set("success", result.success);
        let _ = entry.set("agent", result.agent.clone());

        // If the output is valid JSON, parse it and merge top-level fields
        // into the entry table so conditions can access nested fields directly:
        //   results.parse.hasUnknowns  (parsed field)
        //   results.parse.output       (raw string, still available)
        if let Ok(serde_json::Value::Object(map)) =
            serde_json::from_str::<serde_json::Value>(&result.output)
        {
            for (key, value) in map {
                if let Ok(lua_val) = json_value_to_lua(lua, &value) {
                    let _ = entry.set(key.as_str(), lua_val);
                }
            }
        }

        let _ = ctx.set(name.as_str(), entry);
    }
    let _ = lua.globals().set("results", ctx);

    match lua.load(condition).eval::<mlua::Value>() {
        Ok(mlua::Value::Boolean(b)) => b,
        Ok(mlua::Value::Nil) => false,
        Ok(_) => true,
        Err(e) => {
            tracing::warn!("Condition evaluation failed for '{}': {}", condition, e);
            false
        }
    }
}
