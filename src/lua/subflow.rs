//! `run_flow()` / `crew:subworkflow()` implementation.
//!
//! This module exposes two entry points:
//!
//! * `invoke_subflow` — the async function that actually runs a sub-flow.
//!   Shared by the `crew:subworkflow` method (see `crew_userdata.rs`) and the
//!   sandbox-level `run_flow` global registered here.
//! * `register_run_flow` — called from `sandbox::register_lua_globals` to
//!   install the `run_flow(path, input?)` Lua function on every VM IronCrew
//!   creates. Registration always succeeds; if the VM lacks the required
//!   `Runtime`/`project_dir` app-data (parse-time VMs), the function returns
//!   a clean validation error at call time instead of panicking.
//!
//! # Semantics
//!
//! * `path` is resolved **relative to the calling VM's project directory**.
//!   Absolute paths, `..` segments, empty paths, and paths that escape the
//!   project root (even via symlinks) are rejected before anything runs.
//! * The sub-flow executes in a freshly-constructed
//!   `create_crew_lua_with_lib_dirs` VM seeded with the sub-flow's own `_lib`
//!   directory, with its own `Crew.new`/`Agent.new` constructors. All inter-VM
//!   data transfer goes through JSON — no Lua values cross the boundary.
//! * Depth is tracked through `SubflowDepth` app-data on each VM. Every
//!   invocation increments it; the limit is `IRONCREW_MAX_FLOW_DEPTH`
//!   (default 5) and exceeded calls fail fast with a validation error.
//! * On success the sub-flow's final expression is JSON-bridged back into a
//!   Lua value in the caller's VM. Tables round-trip as tables, primitives
//!   as primitives, everything else collapses to `nil`.

use std::path::PathBuf;
use std::sync::Arc;

use mlua::{Lua, Result as LuaResult, Value};

use crate::engine::conversation_definition::ConversationSourceContext;
use crate::engine::eventbus::{CrewEvent, EventBus};
use crate::engine::runtime::Runtime;
use crate::utils::error::IronCrewError;

use super::json::{
    json_value_to_lua_with_limits, lua_table_to_json_with_limits, lua_value_to_json_with_limits,
};
use super::limits::LuaExecutionGuard;
use super::subflow_setup::prepare_subflow;

/// Newtype stashed in `Lua::app_data` to carry the current sub-flow nesting
/// depth between VMs. Starts at `0` in the top-level VM and increments by one
/// each time `invoke_subflow` is called.
#[derive(Clone, Copy, Debug, Default)]
pub struct SubflowDepth(pub usize);

/// Context threaded into `invoke_subflow`. Carries the shared `Runtime`
/// handle, the caller's project directory (relative to which the sub-flow
/// path is resolved), the current nesting depth, and an optional `EventBus`
/// for telemetry.
pub struct SubflowContext {
    pub runtime: Arc<Runtime>,
    pub project_dir: Arc<PathBuf>,
    pub depth: usize,
    pub eventbus: Option<EventBus>,
    /// Immutable source and lexical directory for HTTP conversations. `None`
    /// preserves the ordinary filesystem-backed CLI/runtime behavior.
    pub source_context: Option<ConversationSourceContext>,
    /// Optional `output_key` — when set, the return value is wrapped in a
    /// single-field table `{ [key] = <serialized sub-flow result> }`. Only
    /// the legacy `crew:subworkflow` API uses this; `run_flow` always passes
    /// `None`.
    pub output_key: Option<String>,
}

/// Resolve the max flow depth used when a new runtime snapshots its policy.
///
/// Public so `AgentAsTool` can share the same cap when guarding
/// agent-as-tool delegation depth (see
/// `docs/superpowers/specs/2026-04-20-agent-as-tool-design.md`).
pub fn max_flow_depth() -> usize {
    std::env::var("IRONCREW_MAX_FLOW_DEPTH")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(5)
}

/// Invoke a sub-flow. This is the shared implementation behind both
/// `crew:subworkflow` and `run_flow`.
///
/// `lua` is the **caller's** VM — we use it only to JSON-bridge the result
/// back out. The sub-flow runs in a fresh VM created via
/// `create_crew_lua_with_lib_dirs`, seeded with the sub-flow directory's `_lib`.
pub async fn invoke_subflow(
    lua: &Lua,
    path: String,
    input_json: Option<serde_json::Value>,
    ctx: &SubflowContext,
) -> LuaResult<Value> {
    // ── Depth cap ──────────────────────────────────────────────────────────
    let cap = ctx.runtime.tool_registry.max_flow_depth();
    if ctx.depth >= cap {
        return Err(mlua::Error::external(IronCrewError::Validation(format!(
            "run_flow depth exceeded: already at {} (limit {})",
            ctx.depth, cap
        ))));
    }
    let json_limits = ctx
        .runtime
        .lua_vm_policy()
        .map_err(mlua::Error::external)?
        .json_limits();

    let span = tracing::info_span!(
        "run_flow",
        path = %path,
        depth = ctx.depth,
    );
    let _enter = span.enter();

    tracing::info!("run_flow invoked: {}", path);
    if let Some(ref bus) = ctx.eventbus {
        bus.emit(CrewEvent::Log {
            level: "info".into(),
            message: format!("run_flow: {}", path),
        });
    }

    // Select one source branch. Conversation execution uses only captured
    // bytes; ordinary CLI/runtime subflows retain the filesystem behavior.
    let prepared = prepare_subflow(&path, ctx)?;
    let sub_lua = prepared.lua;

    // ── Inject input ───────────────────────────────────────────────────────
    if let Some(ref json) = input_json {
        let input_value = json_value_to_lua_with_limits(&sub_lua, json, json_limits)?;
        sub_lua.globals().set("input", input_value)?;
    }

    // ── Execute the sub-flow script ────────────────────────────────────────
    let sub_result: Value = {
        let _execution = LuaExecutionGuard::begin(&sub_lua)?;
        sub_lua
            .load(prepared.script.as_ref())
            .set_name(prepared.source_name)
            .eval_async()
            .await?
    };

    // ── Marshal the result back across VMs via JSON ───────────────────────
    let output = match ctx.output_key.clone() {
        Some(key) => {
            let wrapper = lua.create_table()?;
            let json_str = match sub_result {
                Value::Table(t) => {
                    let json = lua_table_to_json_with_limits(&t, json_limits)?;
                    serde_json::to_string(&json).map_err(|e| {
                        mlua::Error::external(IronCrewError::Validation(format!(
                            "Failed to serialize subworkflow output: {}",
                            e
                        )))
                    })?
                }
                Value::String(s) => s.to_str()?.to_string(),
                _ => String::new(),
            };
            wrapper.set(key, json_str)?;
            Value::Table(wrapper)
        }
        None => match sub_result {
            Value::Table(t) => {
                let json = lua_table_to_json_with_limits(&t, json_limits)?;
                json_value_to_lua_with_limits(lua, &json, json_limits)?
            }
            Value::String(s) => {
                let s = s.to_str()?.to_string();
                Value::String(lua.create_string(&s)?)
            }
            Value::Integer(i) => Value::Integer(i),
            Value::Number(n) => Value::Number(n),
            Value::Boolean(b) => Value::Boolean(b),
            Value::Nil => Value::Nil,
            _ => Value::Nil,
        },
    };

    tracing::info!("run_flow completed: {}", path);
    if let Some(ref bus) = ctx.eventbus {
        bus.emit(CrewEvent::Log {
            level: "info".into(),
            message: format!("run_flow done: {}", path),
        });
    }

    Ok(output)
}

/// Register the sandbox-level `run_flow(path, input?)` global.
///
/// The function works on any Lua VM that has three pieces of app-data seeded:
///   * `Arc<Runtime>` — the runtime whose tool registry + provider get reused.
///   * `Arc<PathBuf>` — the VM's project directory (for path resolution).
///   * `SubflowDepth` — the current nesting depth (defaults to 0 if absent).
///
/// If any required piece is missing (typically on parse-time helper VMs
/// like the ones used to load agent/tool definition files), the call fails
/// with `IronCrewError::Validation` instead of panicking.
pub fn register_run_flow(lua: &Lua) -> LuaResult<()> {
    let run_flow = lua.create_async_function(
        move |lua, (path, input): (String, Option<mlua::Value>)| async move {
            crate::lua::bootstrap::reject_effect(&lua, "run_flow")?;
            // Pull everything out of app-data up front so the borrows drop
            // before we await — mlua's app_data_ref is a RefCell, not Send.
            let runtime = match lua.app_data_ref::<Arc<Runtime>>() {
                Some(r) => r.clone(),
                None => {
                    return Err(mlua::Error::external(IronCrewError::Validation(
                        "run_flow unavailable: no Runtime bound to this Lua VM".into(),
                    )));
                }
            };
            let project_dir = match lua.app_data_ref::<Arc<PathBuf>>() {
                Some(p) => p.clone(),
                None => {
                    return Err(mlua::Error::external(IronCrewError::Validation(
                        "run_flow unavailable: no project_dir bound to this Lua VM".into(),
                    )));
                }
            };
            let depth = lua.app_data_ref::<SubflowDepth>().map(|d| d.0).unwrap_or(0);
            let eventbus = lua.app_data_ref::<EventBus>().map(|e| e.clone());
            let source_context = lua
                .app_data_ref::<ConversationSourceContext>()
                .map(|context| context.clone());
            let json_limits = runtime
                .lua_vm_policy()
                .map_err(mlua::Error::external)?
                .json_limits();

            // Normalize the optional input arg into JSON.
            let input_json: Option<serde_json::Value> = match input {
                Some(Value::Table(t)) => Some(lua_table_to_json_with_limits(&t, json_limits)?),
                Some(Value::Nil) | None => None,
                Some(other) => Some(lua_value_to_json_with_limits(other, json_limits)?),
            };

            let ctx = SubflowContext {
                runtime,
                project_dir,
                depth,
                eventbus,
                source_context,
                output_key: None,
            };

            invoke_subflow(&lua, path, input_json, &ctx).await
        },
    )?;
    lua.globals().set("run_flow", run_flow)?;
    Ok(())
}
