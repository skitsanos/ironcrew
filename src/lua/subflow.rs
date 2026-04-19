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
//! * The sub-flow executes in a freshly-constructed `create_crew_lua()` VM
//!   with its own `Crew.new`/`Agent.new` constructors. All inter-VM data
//!   transfer goes through JSON — no Lua values cross the boundary.
//! * Depth is tracked through `SubflowDepth` app-data on each VM. Every
//!   invocation increments it; the limit is `IRONCREW_MAX_FLOW_DEPTH`
//!   (default 5) and exceeded calls fail fast with a validation error.
//! * On success the sub-flow's final expression is JSON-bridged back into a
//!   Lua value in the caller's VM. Tables round-trip as tables, primitives
//!   as primitives, everything else collapses to `nil`.

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use mlua::{Lua, Result as LuaResult, Value};

use crate::engine::eventbus::{CrewEvent, EventBus};
use crate::engine::runtime::Runtime;
use crate::utils::error::IronCrewError;

use super::api::{register_agent_constructor, register_crew_constructor};
use super::json::{json_value_to_lua, lua_table_to_json};
use super::parsers::load_agents_from_files;
use super::sandbox::create_crew_lua;

/// Default maximum recursive `run_flow` depth. Overridable via the
/// `IRONCREW_MAX_FLOW_DEPTH` environment variable.
const DEFAULT_MAX_FLOW_DEPTH: usize = 5;

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
    /// Optional `output_key` — when set, the return value is wrapped in a
    /// single-field table `{ [key] = <serialized sub-flow result> }`. Only
    /// the legacy `crew:subworkflow` API uses this; `run_flow` always passes
    /// `None`.
    pub output_key: Option<String>,
}

/// Resolve the max flow depth from the environment, falling back to
/// `DEFAULT_MAX_FLOW_DEPTH`. Parsed on each call so tests can adjust the cap
/// via `std::env::set_var` without restarting the process.
fn max_flow_depth() -> usize {
    std::env::var("IRONCREW_MAX_FLOW_DEPTH")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_MAX_FLOW_DEPTH)
}

/// Validate and canonicalize a sub-flow path against the caller's project
/// directory. Returns the canonical absolute path or a validation error.
fn validate_subflow_path(project_dir: &Path, path: &str) -> Result<PathBuf, IronCrewError> {
    let flow_path = Path::new(path);
    if flow_path.as_os_str().is_empty()
        || flow_path.is_absolute()
        || flow_path.components().any(|c| {
            matches!(
                c,
                Component::ParentDir
                    | Component::RootDir
                    | Component::Prefix(_)
                    | Component::CurDir
            )
        })
    {
        return Err(IronCrewError::Validation("Invalid subworkflow path".into()));
    }

    let candidate = project_dir.join(flow_path);
    let base = project_dir
        .canonicalize()
        .unwrap_or_else(|_| project_dir.to_path_buf());
    let canonical = candidate.canonicalize().map_err(|e| {
        IronCrewError::Validation(format!("Failed to resolve subworkflow '{}': {}", path, e))
    })?;

    if !canonical.starts_with(&base) {
        return Err(IronCrewError::Validation(
            "Subworkflow path escapes project directory".into(),
        ));
    }

    if !canonical.is_file() {
        return Err(IronCrewError::Validation(format!(
            "Subworkflow not found: {}",
            canonical.display()
        )));
    }

    Ok(canonical)
}

/// Invoke a sub-flow. This is the shared implementation behind both
/// `crew:subworkflow` and `run_flow`.
///
/// `lua` is the **caller's** VM — we use it only to JSON-bridge the result
/// back out. The sub-flow runs in a fresh VM created via `create_crew_lua()`.
pub async fn invoke_subflow(
    lua: &Lua,
    path: String,
    input_json: Option<serde_json::Value>,
    ctx: &SubflowContext,
) -> LuaResult<Value> {
    // ── Depth cap ──────────────────────────────────────────────────────────
    let cap = max_flow_depth();
    if ctx.depth >= cap {
        return Err(mlua::Error::external(IronCrewError::Validation(format!(
            "run_flow depth exceeded: already at {} (limit {})",
            ctx.depth, cap
        ))));
    }

    // ── Path validation ────────────────────────────────────────────────────
    let flow_path =
        validate_subflow_path(&ctx.project_dir, &path).map_err(mlua::Error::external)?;

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

    // ── Build the sub-flow VM ──────────────────────────────────────────────
    let sub_lua = create_crew_lua().map_err(mlua::Error::external)?;

    // Seed app-data on the child VM so its own `run_flow` calls resolve
    // against the sub-flow's directory (not the parent's).
    let sub_dir = flow_path
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| ctx.project_dir.as_ref().clone());
    let sub_project_dir_arc = Arc::new(sub_dir.clone());
    sub_lua.set_app_data(ctx.runtime.clone());
    sub_lua.set_app_data(sub_project_dir_arc.clone());
    sub_lua.set_app_data(SubflowDepth(ctx.depth + 1));
    if let Some(ref bus) = ctx.eventbus {
        sub_lua.set_app_data(bus.clone());
    }

    register_agent_constructor(&sub_lua)?;

    // Auto-load agents from `<sub_dir>/agents/` (mirrors top-level loader).
    let agents_dir = sub_dir.join("agents");
    let agent_files = if agents_dir.is_dir() {
        std::fs::read_dir(&agents_dir)
            .into_iter()
            .flatten()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("lua"))
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    let sub_agents = load_agents_from_files(&agent_files).map_err(mlua::Error::external)?;

    register_crew_constructor(&sub_lua, ctx.runtime.clone(), sub_agents, sub_dir)?;

    // ── Inject input ───────────────────────────────────────────────────────
    if let Some(ref json) = input_json {
        let input_value = json_value_to_lua(&sub_lua, json)?;
        sub_lua.globals().set("input", input_value)?;
    }

    // ── Execute the sub-flow script ────────────────────────────────────────
    let script = std::fs::read_to_string(&flow_path)
        .map_err(|e| mlua::Error::external(IronCrewError::Io(e)))?;
    let sub_result: Value = sub_lua.load(&script).eval_async().await?;

    // ── Marshal the result back across VMs via JSON ───────────────────────
    let output = match ctx.output_key.clone() {
        Some(key) => {
            let wrapper = lua.create_table()?;
            let json_str = match sub_result {
                Value::Table(t) => {
                    let json = lua_table_to_json(&t)?;
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
                let json = lua_table_to_json(&t)?;
                json_value_to_lua(lua, &json)?
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

            // Normalize the optional input arg into JSON.
            let input_json: Option<serde_json::Value> = match input {
                Some(Value::Table(t)) => Some(lua_table_to_json(&t)?),
                Some(Value::Nil) | None => None,
                Some(other) => Some(crate::lua::json::lua_value_to_json(other)?),
            };

            let ctx = SubflowContext {
                runtime,
                project_dir,
                depth,
                eventbus,
                output_key: None,
            };

            invoke_subflow(&lua, path, input_json, &ctx).await
        },
    )?;
    lua.globals().set("run_flow", run_flow)?;
    Ok(())
}
