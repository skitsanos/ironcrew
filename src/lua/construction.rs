//! Bounded construction evaluation. No execution-capable userdata is exposed.
use std::path::Path;
use std::sync::{Arc, Mutex};

use mlua::{Lua, Table};

use crate::engine::conversation_definition::{ConversationSourceContext, capture_flow_source};
use crate::engine::runtime::Runtime;
use crate::llm::openai::OpenAiProvider;
use crate::utils::error::{IronCrewError, Result};

pub(crate) mod budget;
mod crew;
mod references;
mod vm;

#[derive(Clone, Default)]
pub(crate) struct Evaluation(Arc<Mutex<State>>, Arc<vm::Clock>);

#[derive(Default)]
struct State {
    crews: Vec<mlua::AnyUserData>,
    incomplete: Option<String>,
    declarations: usize,
    nodes: usize,
    bytes: usize,
}

#[derive(Default)]
pub(crate) struct Report {
    pub crews: usize,
    pub agents: usize,
    pub tasks: usize,
}

pub(crate) fn active(lua: &Lua) -> bool {
    lua.app_data_ref::<Evaluation>().is_some()
}

pub(crate) fn block(lua: &Lua, capability: &str) -> mlua::Error {
    mlua::Error::external(incomplete(lua, capability))
}

fn incomplete(lua: &Lua, capability: &str) -> IronCrewError {
    let reason = format!(
        "'{capability}' requires execution or external state; remaining construction was not evaluated"
    );
    if let Some(evaluation) = lua.app_data_ref::<Evaluation>() {
        evaluation
            .0
            .lock()
            .unwrap()
            .incomplete
            .get_or_insert(reason.clone());
    }
    IronCrewError::ValidationIncomplete(reason)
}

pub(crate) fn capture(
    lua: &Lua,
    value: super::crew_userdata::LuaCrew,
) -> mlua::Result<mlua::Value> {
    let state = lua
        .app_data_ref::<Evaluation>()
        .expect("evaluation installed")
        .clone();
    let mut state = state.0.lock().unwrap();
    if state.crews.len() >= 16 {
        return Err(mlua::Error::external("construction exceeds 16 crews"));
    }
    let original = lua.create_userdata(value)?;
    state.crews.push(original.clone());
    Ok(mlua::Value::UserData(
        lua.create_userdata(crew::ConstructionCrew(original))?,
    ))
}

pub(crate) async fn evaluate(path: &Path) -> Result<Report> {
    // Snapshot traversal is bounded and rejects symlinks/special source files.
    // Perform physical reads off the Tokio workers and retain no live read API.
    let path = path.to_path_buf();
    let (snapshot, entrypoint) = tokio::task::spawn_blocking(move || {
        let metadata = std::fs::symlink_metadata(&path)?;
        let (root, entrypoint) = if metadata.is_dir() {
            (path, "crew.lua".into())
        } else if metadata.is_file() {
            (
                path.parent()
                    .filter(|p| !p.as_os_str().is_empty())
                    .unwrap_or(Path::new("."))
                    .to_path_buf(),
                path.file_name()
                    .ok_or_else(|| IronCrewError::Validation("missing Lua filename".into()))?
                    .to_owned(),
            )
        } else {
            return Err(IronCrewError::Validation(
                "validation path must not be a symlink or special file".into(),
            ));
        };
        Ok::<_, IronCrewError>((Arc::new(capture_flow_source(&root)?), entrypoint))
    })
    .await
    .map_err(|_| IronCrewError::Validation("source capture failed".into()))??;
    let source = snapshot.source(Path::new(&entrypoint))?.ok_or_else(|| {
        IronCrewError::Validation("entrypoint missing from source snapshot".into())
    })?;
    let state = Evaluation::default();
    let _cleanup = budget::Cleanup(state.clone());
    let lua = vm::create(state.clone())?;
    let context = ConversationSourceContext::root(snapshot.clone());
    super::snapshot_require::install_snapshot_require(&lua, context.clone())?;
    lua.set_app_data(context);
    // Shared instruction/time/declaration budgets span ALL source VMs.
    let outcome = async {
        if let Some(config) = snapshot.source(Path::new("config.lua"))? {
            let defaults: Table = lua
                .load(config.source())
                .set_name("@config.lua")
                .eval_async()
                .await?;
            budget::admit(&lua, &defaults)?;
            super::parsers::reject_unknown_keys(
                &defaults,
                super::parsers::CREW_KEYS,
                "config.lua",
            )?;
            lua.globals().set("__ironcrew_config_defaults", defaults)?;
        }
        let mut agents = Vec::new();
        for file in snapshot.direct_children(Path::new("agents"))? {
            // Production declaration loaders use a fresh VM per file too;
            // config or another declaration must not alter these globals.
            let declaration_lua = vm::create(state.clone())?;
            let table = declaration_lua
                .load(file.source())
                .set_name(file.relative_path().to_string_lossy())
                .eval_async::<Table>()
                .await?;
            budget::admit(&lua, &table)?;
            agents.push(super::parsers::agent_from_lua_table(&table)?);
        }
        let mut tools = Vec::new();
        for file in snapshot.direct_children(Path::new("tools"))? {
            let declaration_lua = vm::create(state.clone())?;
            let table = declaration_lua
                .load(file.source())
                .set_name(file.relative_path().to_string_lossy())
                .eval_async::<Table>()
                .await?;
            budget::admit(&lua, &table)?;
            let tool = super::parsers::tool_def_from_lua_table(
                &table,
                file.relative_path(),
                file.shared_source(),
            )?;
            if tool.name.starts_with("agent__")
                || tools
                    .iter()
                    .any(|t: &super::parsers::LuaToolDef| t.name == tool.name)
            {
                return Err(IronCrewError::Validation(
                    "duplicate or reserved custom tool name".into(),
                ));
            }
            tools.push(tool);
        }
        validate_sql(&snapshot)?;
        let mut runtime = Runtime::new(
            Box::new(OpenAiProvider::new("validation-only".into(), None)),
            Some(snapshot.root()),
        );
        runtime.register_lua_tools(tools)?;
        let runtime = Arc::new(runtime);
        runtime.set_self_ref(Arc::downgrade(&runtime));
        super::api::register_agent_constructor(&lua)?;
        super::api::register_crew_constructor(
            &lua,
            runtime,
            agents,
            snapshot.root().to_path_buf(),
        )?;
        super::api::set_ironcrew_mode(&lua, "validate")?;
        lua.load(source.source())
            .set_name(source.relative_path().to_string_lossy())
            .exec_async()
            .await?;
        Ok::<_, IronCrewError>(())
    }
    .await;
    let (crews, incomplete) = {
        let mut state = state.0.lock().unwrap();
        (std::mem::take(&mut state.crews), state.incomplete.take())
    };
    // Graph errors discovered before an execution boundary take precedence.
    let report = references::validate(&lua, &crews).await?;
    if let Some(reason) = incomplete {
        return Err(IronCrewError::ValidationIncomplete(reason));
    }
    outcome?;
    if report.crews == 0 {
        return Err(IronCrewError::ValidationIncomplete(
            "no Crew was constructed on the evaluated path".into(),
        ));
    }
    Ok(report)
}

fn validate_sql(
    snapshot: &crate::engine::conversation_definition::FlowSourceSnapshot,
) -> Result<()> {
    #[cfg(feature = "postgres")]
    {
        use crate::engine::app_db::{operations::OperationRegistry, policy::AppDbPolicy};
        let sources = snapshot
            .sql_sources()
            .into_iter()
            .map(|(name, source)| (name, source.to_string()))
            .collect();
        OperationRegistry::from_sources(sources, &AppDbPolicy::capture()?)?;
    }
    #[cfg(not(feature = "postgres"))]
    if !snapshot.sql_sources().is_empty() {
        return Err(IronCrewError::ValidationIncomplete(
            "SQL declarations require a postgres-enabled binary".into(),
        ));
    }
    Ok(())
}
