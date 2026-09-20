use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use mlua::debug::DebugEvent;
use mlua::{HookTriggers, Lua, MultiValue, StdLib, Value, VmState};

use super::{Evaluation, block};

pub(super) struct Clock {
    start: Instant,
    instructions: AtomicU64,
}

impl Default for Clock {
    fn default() -> Self {
        Self {
            start: Instant::now(),
            instructions: AtomicU64::new(0),
        }
    }
}

pub(super) fn create(state: Evaluation) -> mlua::Result<Lua> {
    let lua = Lua::new_with(
        StdLib::STRING | StdLib::TABLE | StdLib::MATH,
        mlua::LuaOptions::default(),
    )?;
    lua.set_memory_limit(32 * 1024 * 1024)?;
    let clock = state.1.clone();
    lua.set_app_data(state);
    lua.set_global_hook(
        HookTriggers::new().on_calls().every_nth_instruction(1_000),
        move |lua, debug| {
            let instructions = if debug.event() == DebugEvent::Count {
                clock.instructions.fetch_add(1_000, Ordering::Relaxed) + 1_000
            } else {
                clock.instructions.load(Ordering::Relaxed)
            };
            if instructions > 2_000_000 || clock.start.elapsed() > Duration::from_secs(5) {
                return Err(mlua::Error::external(
                    "construction evaluation instruction/time limit exceeded",
                ));
            }
            if lua.inspect_stack(64, |_| ()).is_some() {
                return Err(mlua::Error::external(
                    "construction evaluation recursion limit exceeded (64 frames)",
                ));
            }
            Ok(VmState::Continue)
        },
    )?;
    // Catching a limit exception could otherwise resume an unbounded loop.
    // No coroutine or protected-call facility is available in this VM.
    for name in [
        "pcall",
        "xpcall",
        "load",
        "loadfile",
        "dofile",
        "collectgarbage",
        "setmetatable",
        "env",
        "run_flow",
        "uuid4",
        "now_rfc3339",
        "now_unix_ms",
        "require",
    ] {
        lua.globals().set(
            name,
            lua.create_function(move |lua, _: MultiValue| -> mlua::Result<()> {
                Err(block(lua, name))
            })?,
        )?;
    }
    for name in [
        "http",
        "fs",
        "io",
        "os",
        "postgres",
        "llm",
        "package",
        "coroutine",
        "crypto",
        "regex",
    ] {
        let table = lua.create_table()?;
        let meta = lua.create_table()?;
        meta.set(
            "__index",
            lua.create_function(move |lua, _: MultiValue| -> mlua::Result<()> {
                Err(block(lua, name))
            })?,
        )?;
        table.set_metatable(Some(meta))?;
        lua.globals().set(name, table)?;
    }
    // Suppress script output; it is neither validation evidence nor a result.
    lua.globals()
        .set("print", lua.create_function(|_, _: MultiValue| Ok(()))?)?;
    lua.globals().set(
        "json_stringify",
        lua.create_function(|_, value: Value| {
            let json = crate::lua::json::lua_value_to_json(value)?;
            serde_json::to_string(&json).map_err(mlua::Error::external)
        })?,
    )?;
    lua.globals().set(
        "json_parse",
        lua.create_function(|lua, source: mlua::LuaString| {
            if source.as_bytes().len() > 1024 * 1024 {
                return Err(mlua::Error::external("json_parse input exceeds 1 MiB"));
            }
            let value = serde_json::from_slice(source.as_bytes().as_ref())
                .map_err(mlua::Error::external)?;
            crate::lua::json::json_value_to_lua(lua, &value)
        })?,
    )?;
    Ok(lua)
}
