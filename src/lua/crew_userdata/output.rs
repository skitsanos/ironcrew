use crate::engine::task::TaskResult;

pub(super) fn results_to_lua(lua: &mlua::Lua, results: &[TaskResult]) -> mlua::Result<mlua::Table> {
    // Convert results to Lua table
    let results_table = lua.create_table()?;
    for (i, result) in results.iter().enumerate() {
        let entry = lua.create_table()?;
        entry.set("task", result.task.clone())?;
        entry.set("agent", result.agent.clone())?;
        entry.set("output", result.output.clone())?;
        entry.set("success", result.success)?;
        entry.set("duration_ms", result.duration_ms)?;
        if let Some(ref usage) = result.token_usage {
            let usage_table = lua.create_table()?;
            usage_table.set("prompt_tokens", usage.prompt_tokens)?;
            usage_table.set("completion_tokens", usage.completion_tokens)?;
            usage_table.set("total_tokens", usage.total_tokens)?;
            usage_table.set("cached_tokens", usage.cached_tokens)?;
            entry.set("token_usage", usage_table)?;
        }
        results_table.set(i + 1, entry)?;
    }

    Ok(results_table)
}
