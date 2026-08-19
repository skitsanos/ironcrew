use std::collections::HashMap;

use crate::engine::task::TaskResult;
use crate::lua::json::json_value_to_lua;
use crate::lua::limits::{LuaExecutionGuard, LuaLimits};
use crate::lua::sandbox::{create_eval_lua, fresh_eval_environment};

pub fn evaluate_condition(condition: &str, results: &HashMap<String, TaskResult>) -> bool {
    let lua = match LuaLimits::from_env()
        .map_err(|error| error.to_string())
        .and_then(|limits| create_eval_lua(limits).map_err(|error| error.to_string()))
    {
        Ok(lua) => lua,
        Err(error) => {
            tracing::error!(%error, "Condition Lua VM could not be initialized");
            return false;
        }
    };
    evaluate_condition_inner(&lua, condition, results)
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
    // Bind `results` in a per-call environment rather than on the shared
    // globals, so one flow's condition cannot observe another's context.
    let Ok(environment) = fresh_eval_environment(lua) else {
        return false;
    };
    if environment.set("results", ctx).is_err() {
        return false;
    }

    match lua
        .load(condition)
        .set_environment(environment)
        .eval::<mlua::Value>()
    {
        Ok(mlua::Value::Boolean(b)) => b,
        Ok(mlua::Value::Nil) => false,
        Ok(_) => true,
        Err(e) => {
            tracing::warn!("Condition evaluation failed for '{}': {}", condition, e);
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn results() -> HashMap<String, TaskResult> {
        HashMap::new()
    }

    #[test]
    fn conditions_cannot_reach_io_os_execute_or_package() {
        for probe in [
            "return io ~= nil",
            "return os.execute ~= nil",
            "return os.getenv ~= nil",
            "return os.exit ~= nil",
            "return os.remove ~= nil",
            "return package ~= nil",
            "return require ~= nil",
            "return loadfile ~= nil",
            "return dofile ~= nil",
        ] {
            assert!(
                !evaluate_condition(probe, &results()),
                "condition reached {probe}"
            );
        }
    }

    #[test]
    fn conditions_keep_permitted_os_helpers() {
        assert!(evaluate_condition(
            "return os.time ~= nil and os.clock ~= nil",
            &results()
        ));
    }

    #[test]
    fn condition_globals_do_not_leak_into_later_conditions() {
        assert!(evaluate_condition(
            "smuggled = true; return true",
            &results()
        ));
        assert!(
            !evaluate_condition("return smuggled == true", &results()),
            "a condition observed a global left by an earlier condition"
        );
    }

    #[test]
    fn condition_shared_global_escape_hatches_do_not_leak() {
        assert!(evaluate_condition(
            "_G.smuggled = true; os.smuggled = true; return true",
            &results()
        ));
        assert!(!evaluate_condition(
            "return _G.smuggled == true or os.smuggled == true",
            &results()
        ));
    }

    #[test]
    fn condition_results_context_is_visible() {
        let mut map = results();
        map.insert(
            "parse".to_string(),
            TaskResult {
                task: "parse".to_string(),
                agent: "analyst".to_string(),
                output: r#"{"hasUnknowns": true}"#.to_string(),
                success: true,
                duration_ms: 0,
                token_usage: None,
                reasoning: None,
            },
        );
        assert!(evaluate_condition("return results.parse.success", &map));
        assert!(evaluate_condition("return results.parse.hasUnknowns", &map));
        assert!(evaluate_condition(
            "return results.parse.agent == 'analyst'",
            &map
        ));
    }
}
