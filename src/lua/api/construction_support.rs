use super::*;

pub(super) fn merge_defaults(lua: &Lua, table: &Table) -> LuaResult<()> {
    if let Ok(defaults) = lua.globals().get::<Table>("__ironcrew_config_defaults") {
        for pair in defaults.pairs::<Value, Value>() {
            let (key, value) = pair?;
            if let Value::String(ref s) = key
                && !table.contains_key(s.clone())?
            {
                table.set(key, value)?;
            }
        }
    }
    #[cfg(not(feature = "mcp"))]
    if crate::lua::construction::active(lua) && table.contains_key("mcp_servers")? {
        return Err(crate::lua::construction::block(
            lua,
            "MCP declarations without the mcp feature",
        ));
    }
    crate::lua::construction::budget::admit(lua, table)
}

pub(super) async fn memory(
    lua: &Lua,
    mode: &str,
    project_dir: &std::path::Path,
    config: MemoryConfig,
) -> LuaResult<MemoryStore> {
    // Validation exposes no memory methods or execution results. Persistent
    // declarations are checked without opening, reading or creating a store.
    if mode == "persistent" && !crate::lua::construction::active(lua) {
        let path = project_dir.join(".ironcrew").join("memory.json");
        MemoryStore::persistent_with_config_async(path, config)
            .await
            .map_err(mlua::Error::external)
    } else {
        Ok(MemoryStore::ephemeral_with_config(config))
    }
}
