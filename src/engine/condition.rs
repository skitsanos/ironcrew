use std::collections::HashMap;

use crate::engine::task::TaskResult;

pub fn evaluate_condition(condition: &str, results: &HashMap<String, TaskResult>) -> bool {
    let lua = mlua::Lua::new();

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
