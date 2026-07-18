//! Resource and execution limits shared by every IronCrew Lua VM.
//!
//! Memory is capped for the lifetime of the VM. Instruction and wall-clock
//! budgets are activated per top-level execution with [`LuaExecutionGuard`].
//! Keeping those budgets inactive while a VM is idle is important for
//! persistent conversation VMs: an old session must not expire merely because
//! nobody talked to it for a while.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use mlua::{HookTriggers, Lua, Result as LuaResult, VmState};

const DEFAULT_MEMORY_BYTES: usize = 32 * 1024 * 1024;
const MIN_MEMORY_BYTES: usize = 1024 * 1024;
const MAX_MEMORY_BYTES: usize = 512 * 1024 * 1024;

const DEFAULT_MAX_INSTRUCTIONS: u64 = 50_000_000;
const MIN_MAX_INSTRUCTIONS: u64 = 100_000;
const MAX_MAX_INSTRUCTIONS: u64 = 10_000_000_000;

// Matches the API's default maximum run lifetime. Operators can lower this
// independently for workloads that should never retain a Lua execution slot
// for 30 minutes.
const DEFAULT_MAX_EXECUTION_SECONDS: u64 = 1_800;
const MAX_MAX_EXECUTION_SECONDS: u64 = 86_400;

// Frequent enough to stop a hot loop promptly without the high overhead of a
// per-line or per-instruction hook.
const HOOK_INTERVAL: u32 = 10_000;

/// Effective limits installed on a Lua VM.
#[derive(Clone, Copy, Debug)]
pub struct LuaLimits {
    pub max_memory_bytes: usize,
    pub max_instructions: u64,
    pub max_execution_time: Duration,
}

impl Default for LuaLimits {
    fn default() -> Self {
        Self {
            max_memory_bytes: DEFAULT_MEMORY_BYTES,
            max_instructions: DEFAULT_MAX_INSTRUCTIONS,
            max_execution_time: Duration::from_secs(DEFAULT_MAX_EXECUTION_SECONDS),
        }
    }
}

impl LuaLimits {
    /// Read and strictly validate the process-level Lua resource settings.
    pub fn from_env() -> LuaResult<Self> {
        Ok(Self {
            max_memory_bytes: parse_bounded_env(
                "IRONCREW_LUA_MAX_MEMORY_BYTES",
                DEFAULT_MEMORY_BYTES,
                MIN_MEMORY_BYTES,
                MAX_MEMORY_BYTES,
            )?,
            max_instructions: parse_bounded_env(
                "IRONCREW_LUA_MAX_INSTRUCTIONS",
                DEFAULT_MAX_INSTRUCTIONS,
                MIN_MAX_INSTRUCTIONS,
                MAX_MAX_INSTRUCTIONS,
            )?,
            max_execution_time: Duration::from_secs(parse_bounded_env(
                "IRONCREW_LUA_MAX_EXECUTION_SECONDS",
                DEFAULT_MAX_EXECUTION_SECONDS,
                1,
                MAX_MAX_EXECUTION_SECONDS,
            )?),
        })
    }
}

fn parse_bounded_env<T>(name: &str, default: T, min: T, max: T) -> LuaResult<T>
where
    T: Copy + std::fmt::Display + std::str::FromStr + PartialOrd,
{
    let Some(raw) = std::env::var(name).ok() else {
        return Ok(default);
    };
    let value = raw.parse::<T>().map_err(|_| {
        mlua::Error::external(format!(
            "{name} must be a whole number between {min} and {max}"
        ))
    })?;
    if value < min || value > max {
        return Err(mlua::Error::external(format!(
            "{name} must be between {min} and {max}; got {value}"
        )));
    }
    Ok(value)
}

#[derive(Debug, Default)]
struct ExecutionState {
    guard_depth: usize,
    started_at: Option<Instant>,
    instructions: u64,
}

#[derive(Clone, Debug)]
struct LuaExecutionController {
    limits: LuaLimits,
    state: Arc<Mutex<ExecutionState>>,
}

impl LuaExecutionController {
    fn enter(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.guard_depth == 0 {
            state.started_at = Some(Instant::now());
            state.instructions = 0;
        }
        state.guard_depth = state.guard_depth.saturating_add(1);
    }

    fn leave(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.guard_depth = state.guard_depth.saturating_sub(1);
        if state.guard_depth == 0 {
            state.started_at = None;
            state.instructions = 0;
        }
    }

    fn hook(&self) -> LuaResult<VmState> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.guard_depth == 0 {
            return Ok(VmState::Continue);
        }
        if state
            .started_at
            .is_some_and(|started| started.elapsed() >= self.limits.max_execution_time)
        {
            return Err(mlua::Error::runtime(format!(
                "Lua execution exceeded wall-clock limit of {} seconds",
                self.limits.max_execution_time.as_secs()
            )));
        }

        state.instructions = state.instructions.saturating_add(u64::from(HOOK_INTERVAL));
        if state.instructions > self.limits.max_instructions {
            return Err(mlua::Error::runtime(format!(
                "Lua execution exceeded instruction limit of {}",
                self.limits.max_instructions
            )));
        }
        Ok(VmState::Continue)
    }
}

/// Activates instruction and wall-clock limits for one top-level Lua call.
///
/// Nested guards share the outer budget. Dropping the outermost guard clears
/// all execution state so a later call on a persistent VM starts fresh.
#[derive(Debug)]
pub struct LuaExecutionGuard {
    controller: LuaExecutionController,
}

impl LuaExecutionGuard {
    pub fn begin(lua: &Lua) -> LuaResult<Self> {
        let controller = lua
            .app_data_ref::<LuaExecutionController>()
            .map(|controller| controller.clone())
            .ok_or_else(|| mlua::Error::runtime("Lua execution limits are not installed"))?;
        controller.enter();
        Ok(Self { controller })
    }
}

impl Drop for LuaExecutionGuard {
    fn drop(&mut self) {
        self.controller.leave();
    }
}

/// Install the lifetime memory cap and the inactive per-execution hook.
pub fn install_lua_limits(lua: &Lua, limits: LuaLimits) -> LuaResult<()> {
    lua.set_memory_limit(limits.max_memory_bytes)?;

    let controller = LuaExecutionController {
        limits,
        state: Arc::new(Mutex::new(ExecutionState::default())),
    };
    let hook_controller = controller.clone();
    lua.set_global_hook(
        HookTriggers::new().every_nth_instruction(HOOK_INTERVAL),
        move |_, _| hook_controller.hook(),
    )?;
    lua.set_app_data(controller);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_limits() -> LuaLimits {
        LuaLimits {
            max_memory_bytes: 2 * 1024 * 1024,
            max_instructions: 100_000,
            max_execution_time: Duration::from_secs(1),
        }
    }

    #[test]
    fn memory_limit_rejects_oversized_allocation() {
        let lua = Lua::new();
        install_lua_limits(&lua, test_limits()).unwrap();

        let error = lua
            .load("return string.rep('x', 4 * 1024 * 1024)")
            .eval::<mlua::LuaString>()
            .expect_err("allocation must exceed the Lua heap limit");
        assert!(
            matches!(error, mlua::Error::MemoryError(_)),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn instruction_guard_terminates_non_yielding_loop() {
        let lua = Lua::new();
        install_lua_limits(&lua, test_limits()).unwrap();
        let _guard = LuaExecutionGuard::begin(&lua).unwrap();

        let error = lua
            .load("while true do end")
            .exec()
            .expect_err("infinite loop must be interrupted");
        assert!(error.to_string().contains("instruction limit"));
    }

    #[test]
    fn guard_drop_resets_deadline_for_idle_vm() {
        let lua = Lua::new();
        let limits = LuaLimits {
            max_memory_bytes: 2 * 1024 * 1024,
            max_instructions: 1_000_000,
            max_execution_time: Duration::from_millis(10),
        };
        install_lua_limits(&lua, limits).unwrap();

        drop(LuaExecutionGuard::begin(&lua).unwrap());
        std::thread::sleep(Duration::from_millis(20));

        let _guard = LuaExecutionGuard::begin(&lua).unwrap();
        let result: i64 = lua
            .load("local x = 0; for i = 1, 20000 do x = x + i end; return x")
            .eval()
            .expect("idle time before a new guard must not consume its budget");
        assert!(result > 0);
    }
}
