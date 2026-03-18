use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use mlua::{Lua, Result as LuaResult, StdLib, Value};

use crate::lua::api::{json_value_to_lua, lua_value_to_json};

/// Register utility global functions available in all Lua sandboxes.
fn register_lua_globals(lua: &Lua) -> LuaResult<()> {
    // env()
    let env_fn = lua.create_function(|_, name: String| Ok(std::env::var(&name).ok()))?;
    lua.globals().set("env", env_fn)?;

    // uuid4()
    let uuid_fn = lua.create_function(|_, ()| Ok(uuid::Uuid::new_v4().to_string()))?;
    lua.globals().set("uuid4", uuid_fn)?;

    // now_rfc3339()
    let now_rfc3339_fn = lua.create_function(|_, ()| Ok(chrono::Utc::now().to_rfc3339()))?;
    lua.globals().set("now_rfc3339", now_rfc3339_fn)?;

    // now_unix_ms()
    let now_unix_ms_fn = lua.create_function(|_, ()| {
        Ok(std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64)
    })?;
    lua.globals().set("now_unix_ms", now_unix_ms_fn)?;

    // json_parse(str) -> Lua value
    let json_parse_fn = lua.create_function(|lua, s: String| {
        let value: serde_json::Value =
            serde_json::from_str(&s).map_err(mlua::Error::external)?;
        json_value_to_lua(lua, &value)
    })?;
    lua.globals().set("json_parse", json_parse_fn)?;

    // json_stringify(value) -> JSON string
    let json_stringify_fn = lua.create_function(|_, value: Value| {
        let json = lua_value_to_json(value)?;
        serde_json::to_string(&json).map_err(mlua::Error::external)
    })?;
    lua.globals().set("json_stringify", json_stringify_fn)?;

    // base64_encode(str)
    let b64_encode_fn =
        lua.create_function(|_, s: String| Ok(STANDARD.encode(s.as_bytes())))?;
    lua.globals().set("base64_encode", b64_encode_fn)?;

    // base64_decode(str)
    let b64_decode_fn = lua.create_function(|_, s: String| {
        let bytes = STANDARD.decode(&s).map_err(mlua::Error::external)?;
        String::from_utf8(bytes).map_err(mlua::Error::external)
    })?;
    lua.globals().set("base64_decode", b64_decode_fn)?;

    // log(level, msg...)
    let log_fn = lua.create_function(|_, args: mlua::Variadic<String>| {
        let args: Vec<String> = args.into_iter().collect();
        if args.is_empty() {
            return Ok(());
        }

        let (level, message) = if args.len() >= 2 {
            let lvl = args[0].as_str();
            let msg = args[1..].join(" ");
            (lvl.to_string(), msg)
        } else {
            ("info".to_string(), args[0].clone())
        };

        match level.as_str() {
            "trace" => tracing::trace!("<lua> {}", message),
            "debug" => tracing::debug!("<lua> {}", message),
            "info" => tracing::info!("<lua> {}", message),
            "warn" => tracing::warn!("<lua> {}", message),
            "error" => tracing::error!("<lua> {}", message),
            _ => tracing::info!("<lua> {}", message),
        }
        Ok(())
    })?;
    lua.globals().set("log", log_fn)?;

    Ok(())
}

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

    register_lua_globals(&lua)?;

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

    register_lua_globals(&lua)?;

    // Register fs namespace (thin wrappers around Rust)
    let fs_table = lua.create_table()?;
    let fs_read = lua.create_function(|_, path: String| {
        std::fs::read_to_string(&path).map_err(mlua::Error::external)
    })?;
    let fs_write = lua.create_function(|_, (path, content): (String, String)| {
        std::fs::write(&path, &content).map_err(mlua::Error::external)
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
