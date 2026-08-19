//! Sandboxed VM for flow-supplied expressions (IC-021).
//!
//! Task conditions and `before_task`/`after_task` hook bytecode come from the
//! flow definition and are untrusted, so they must not run in a default mlua VM
//! (which exposes `io`, `os`, and `package`).

use mlua::{Lua, Result as LuaResult, StdLib};

use crate::lua::limits::install_lua_limits;

/// Create a minimal VM for evaluating flow-supplied expressions (task
/// conditions and `before_task`/`after_task` hook bytecode).
///
/// These chunks come from the flow definition and are untrusted, so the VM is
/// built with the same restricted standard library as the crew VM: no `io`, no
/// `package`, `os` trimmed to clock/time/date/difftime, and no
/// `loadfile`/`dofile`. It deliberately omits the crew globals (`http`, `fs`,
/// `env`, ...): expression evaluation has no legitimate need for effects, and
/// hook bytecode is dumped without upvalues so it could not reach them anyway.
///
/// Callers must create a new VM for every evaluation and run the chunk in a
/// fresh environment (see [`fresh_eval_environment`]). A fresh table alone is
/// insufficient because `_G` and mutable standard-library tables would still
/// expose a reused VM's globals through the environment metatable.
pub(crate) fn create_eval_lua(limits: crate::lua::limits::LuaLimits) -> LuaResult<Lua> {
    let lua = Lua::new_with(
        StdLib::STRING | StdLib::TABLE | StdLib::MATH | StdLib::COROUTINE | StdLib::OS,
        mlua::LuaOptions::default(),
    )?;
    install_lua_limits(&lua, limits)?;

    // Same os trimming and global removal as the crew VM.
    lua.load(
        r#"
        local _os = os
        os = {
            clock = _os.clock,
            time = _os.time,
            date = _os.date,
            difftime = _os.difftime,
        }
        loadfile = nil
        dofile = nil
        "#,
    )
    .exec()?;

    Ok(lua)
}

/// Build a per-evaluation environment table for a VM from [`create_eval_lua`].
///
/// Reads fall through to this evaluation's VM globals so the restricted
/// standard library stays available, while ordinary writes land on the
/// returned table. The VM itself is never reused, so writes through `_G`, the
/// metatable, or mutable library tables are discarded with the evaluation.
pub(crate) fn fresh_eval_environment(lua: &Lua) -> LuaResult<mlua::Table> {
    let env = lua.create_table()?;
    let metatable = lua.create_table()?;
    metatable.set("__index", lua.globals())?;
    env.set_metatable(Some(metatable))?;
    env.set("_G", env.clone())?;
    Ok(env)
}
