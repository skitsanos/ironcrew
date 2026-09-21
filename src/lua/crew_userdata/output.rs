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
        use mlua::LuaSerdeExt;
        entry.set("usage", lua.to_value(&result.usage)?)?;
        results_table.set(i + 1, entry)?;
    }

    Ok(results_table)
}
