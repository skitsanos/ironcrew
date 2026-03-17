use mlua::{Lua, Result as LuaResult, StdLib};

/// Create a Lua VM for crew.lua (full access context).
pub fn create_crew_lua() -> LuaResult<Lua> {
    let lua = Lua::new_with(
        StdLib::STRING | StdLib::TABLE | StdLib::MATH | StdLib::COROUTINE | StdLib::OS,
        mlua::LuaOptions::default(),
    )?;

    // Block dangerous os functions, keep os.clock and os.time
    lua.load(
        r#"
        local _os = os
        os = {
            clock = _os.clock,
            time = _os.time,
            date = _os.date,
            difftime = _os.difftime,
        }
        "#,
    )
    .exec()?;

    // Remove dangerous globals
    lua.load(
        r#"
        loadfile = nil
        dofile = nil
        "#,
    )
    .exec()?;

    Ok(lua)
}

/// Create a restricted Lua VM for tool execute functions.
/// Registers sandbox API: env(), and placeholders for llm, http, fs
/// (full llm/http/fs sandbox APIs will be wired when the tool is executed
/// with a provider context — see LuaScriptTool::execute).
pub fn create_tool_lua() -> LuaResult<Lua> {
    let lua = Lua::new_with(
        StdLib::STRING | StdLib::TABLE | StdLib::MATH,
        mlua::LuaOptions::default(),
    )?;

    // Remove any potentially dangerous globals
    lua.load(
        r#"
        loadfile = nil
        dofile = nil
        require = nil
        os = nil
        io = nil
        "#,
    )
    .exec()?;

    // Register env() in tool context
    let env_fn = lua.create_function(|_, name: String| {
        Ok(std::env::var(&name).ok())
    })?;
    lua.globals().set("env", env_fn)?;

    // Register fs namespace (thin wrappers around Rust)
    let fs_table = lua.create_table()?;
    let fs_read = lua.create_function(|_, path: String| {
        std::fs::read_to_string(&path)
            .map_err(|e| mlua::Error::external(e))
    })?;
    let fs_write = lua.create_function(|_, (path, content): (String, String)| {
        std::fs::write(&path, &content)
            .map_err(|e| mlua::Error::external(e))
    })?;
    fs_table.set("read", fs_read)?;
    fs_table.set("write", fs_write)?;
    lua.globals().set("fs", fs_table)?;

    // Note: llm and http namespaces require async and a provider/client reference.
    // These are registered per-execution in LuaScriptTool::execute when a provider
    // is available. For v1, Lua tools that need llm:chat() or http:get() will need
    // to be wired in a future iteration with the async tool sandbox.

    Ok(lua)
}
