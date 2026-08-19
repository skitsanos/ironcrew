//! Source selection and VM construction for `run_flow`.

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use mlua::Lua;

use super::api::{register_agent_constructor, register_crew_constructor};
use super::parsers::{load_agents_from_files, load_agents_from_snapshot};
use super::sandbox::create_crew_lua_with_policy;
use super::subflow::{SubflowContext, SubflowDepth};
use crate::engine::conversation_definition::ConversationSourceContext;
use crate::utils::error::IronCrewError;

pub(super) struct PreparedSubflow {
    pub lua: Lua,
    pub script: Arc<str>,
    pub source_name: String,
}

pub(super) fn prepare_subflow(
    path: &str,
    context: &SubflowContext,
) -> mlua::Result<PreparedSubflow> {
    validate_requested_path(path).map_err(mlua::Error::external)?;
    match context.source_context.as_ref() {
        Some(source) => prepare_snapshot_subflow(path, context, source),
        None => prepare_filesystem_subflow(path, context),
    }
}

fn prepare_snapshot_subflow(
    path: &str,
    context: &SubflowContext,
    source_context: &ConversationSourceContext,
) -> mlua::Result<PreparedSubflow> {
    let source = source_context
        .source(path)
        .map_err(mlua::Error::external)?
        .ok_or_else(|| {
            mlua::Error::external(IronCrewError::Validation(format!(
                "Subworkflow not found in immutable source: {path}"
            )))
        })?;
    let child_context = source_context
        .child_for_source(&source)
        .map_err(mlua::Error::external)?;
    let sub_dir = child_context
        .snapshot
        .root()
        .join(child_context.logical_dir());
    let lua = create_crew_lua_with_policy(
        Vec::new(),
        context
            .runtime
            .lua_vm_policy()
            .map_err(mlua::Error::external)?,
    )?;
    super::snapshot_require::install_snapshot_require(&lua, child_context.clone())?;
    let agents = child_context
        .direct_children("agents")
        .map_err(mlua::Error::external)?;
    let agents = load_agents_from_snapshot(&agents).map_err(mlua::Error::external)?;
    finish_vm(&lua, context, &sub_dir, Some(child_context), agents)?;
    Ok(PreparedSubflow {
        lua,
        script: source.shared_source(),
        source_name: format!("@snapshot/{}", source.relative_path().display()),
    })
}

fn prepare_filesystem_subflow(
    path: &str,
    context: &SubflowContext,
) -> mlua::Result<PreparedSubflow> {
    let flow_path =
        resolve_filesystem_path(&context.project_dir, path).map_err(mlua::Error::external)?;
    let sub_dir = flow_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| context.project_dir.as_ref().clone());
    let lua = create_crew_lua_with_policy(
        vec![sub_dir.join("_lib")],
        context
            .runtime
            .lua_vm_policy()
            .map_err(mlua::Error::external)?,
    )?;
    let agent_files = discover_agents(&sub_dir.join("agents"));
    let agents = load_agents_from_files(&agent_files).map_err(mlua::Error::external)?;
    finish_vm(&lua, context, &sub_dir, None, agents)?;
    let script = crate::lua::source::read_lua_source(&flow_path)
        .map(Arc::from)
        .map_err(mlua::Error::external)?;
    Ok(PreparedSubflow {
        lua,
        script,
        source_name: format!("@{}", flow_path.display()),
    })
}

fn finish_vm(
    lua: &Lua,
    context: &SubflowContext,
    sub_dir: &Path,
    source_context: Option<ConversationSourceContext>,
    agents: Vec<crate::engine::agent::Agent>,
) -> mlua::Result<()> {
    lua.set_app_data(context.runtime.clone());
    lua.set_app_data(Arc::new(sub_dir.to_path_buf()));
    lua.set_app_data(SubflowDepth(context.depth + 1));
    if let Some(source_context) = source_context {
        lua.set_app_data(source_context);
    }
    if let Some(bus) = &context.eventbus {
        lua.set_app_data(bus.clone());
    }
    register_agent_constructor(lua)?;
    register_crew_constructor(lua, context.runtime.clone(), agents, sub_dir.to_path_buf())?;
    // v1: sub-flows get a fail-closed stub only, never live postgres.*
    // capability -- a diagnosable error instead of a raw nil-global crash.
    // Live capability inside run_flow sub-flows is an explicit follow-up.
    crate::lua::postgres::register_postgres_stub(lua, crate::lua::postgres::STUB_SUBFLOW)
}

fn validate_requested_path(path: &str) -> Result<(), IronCrewError> {
    let path = Path::new(path);
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(IronCrewError::Validation("Invalid subworkflow path".into()));
    }
    Ok(())
}

fn resolve_filesystem_path(project_dir: &Path, path: &str) -> Result<PathBuf, IronCrewError> {
    let candidate = project_dir.join(path);
    let base = project_dir
        .canonicalize()
        .unwrap_or_else(|_| project_dir.to_path_buf());
    let canonical = candidate.canonicalize().map_err(|error| {
        IronCrewError::Validation(format!("Failed to resolve subworkflow '{path}': {error}"))
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

fn discover_agents(directory: &Path) -> Vec<PathBuf> {
    if !directory.is_dir() {
        return Vec::new();
    }
    let mut files = std::fs::read_dir(directory)
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|value| value.to_str()) == Some("lua"))
        .collect::<Vec<_>>();
    files.sort();
    files
}
