use std::cell::RefCell;
use std::collections::HashMap;

use crate::engine::task::TaskResult;
use crate::lua::json::json_value_to_lua;

// Thread-local Lua VM reused for condition evaluation.
thread_local! {
    static CONDITION_LUA: RefCell<mlua::Lua> = RefCell::new(mlua::Lua::new());
}

pub fn evaluate_condition(condition: &str, results: &HashMap<String, TaskResult>) -> bool {
    CONDITION_LUA.with(|cell| evaluate_condition_inner(&cell.borrow(), condition, results))
}

fn evaluate_condition_inner(
    lua: &mlua::Lua,
    condition: &str,
    results: &HashMap<String, TaskResult>,
) -> bool {
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
