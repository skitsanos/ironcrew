use std::path::Path;
use std::sync::Arc;

use crate::engine::conversation_definition::{
    ConversationSourceContext, FlowSourceRoles, FlowSourceSnapshot, SnapshotLuaSource,
};
use crate::engine::runtime::Runtime;
use crate::llm::openai::OpenAiProvider;
use crate::lua::api::{
    load_agents_from_files, load_tool_defs_from_files, register_agent_constructor,
    register_crew_constructor, set_ironcrew_mode,
};
use crate::lua::limits::LuaExecutionGuard;
use crate::lua::loader::ProjectLoader;
use crate::lua::sandbox::create_crew_lua_with_policy;
use crate::tools::runtime_policy::RuntimeExecutionPolicy;
use crate::utils::error::{IronCrewError, Result};

/// Load `.env` files into the process environment.
///
/// Call this **exactly once, before the async runtime starts** (see `main`).
/// `dotenvy` calls `std::env::set_var` internally, which is only sound while the
/// process is single-threaded — calling it from a live request handler on the
/// multithreaded runtime is undefined behavior (that is why `load_project` no
/// longer touches the environment).
///
/// Loads the CWD `.env` first, then the project directory's `.env` if a project
/// path is given. `dotenvy` never overrides a variable that is already set, so
/// values already present in the process environment (or in the CWD `.env`) win
/// over the project `.env`.
pub fn load_dotenv(project_path: Option<&Path>) {
    dotenvy::dotenv().ok();

    if let Some(path) = project_path {
        let project_dir = if path.is_file() {
            path.parent().unwrap_or(Path::new("."))
        } else {
            path
        };
        let env_file = project_dir.join(".env");
        if env_file.exists() {
            dotenvy::from_path(&env_file).ok();
        }
    }
}

/// Load a project's structure from a path (file or directory).
///
/// Does **not** touch the environment — `.env` loading happens once at startup
/// via [`load_dotenv`]. Safe to call from request handlers on the async runtime.
pub fn load_project(path: &Path) -> Result<ProjectLoader> {
    if path.is_file() {
        ProjectLoader::from_file(path)
    } else {
        ProjectLoader::from_directory(path)
    }
}

/// Set up a fully configured Lua VM and Runtime from a loaded project.
///
/// This encapsulates the common pattern of:
/// 1. Creating the Lua sandbox
/// 2. Registering the Agent() constructor
/// 3. Creating the LLM provider from environment variables
/// 4. Loading agents and tools from the project
/// 5. Building the Runtime with Lua tools
/// 6. Registering Crew.new() with preloaded agents
///
/// Returns the configured Lua VM and the shared Runtime.
pub fn setup_crew_runtime(loader: &ProjectLoader) -> Result<(mlua::Lua, Arc<Runtime>)> {
    let (lua, runtime, _) = setup_crew_runtime_inner(loader, None)?;
    Ok((lua, runtime))
}

/// Set up the runtime used only to discover an HTTP conversation definition.
/// The purity marker is installed before config/definition evaluation and is
/// left in place for the caller to remove after the entrypoint returns.
pub(crate) fn setup_http_conversation_runtime(
    loader: &ProjectLoader,
    snapshot: Arc<FlowSourceSnapshot>,
) -> Result<(mlua::Lua, Arc<Runtime>, SnapshotLuaSource)> {
    if snapshot.root() != loader.project_dir() {
        return Err(IronCrewError::Validation(
            "conversation source snapshot does not belong to the loaded flow".into(),
        ));
    }
    let roles = snapshot.roles()?;
    let (lua, runtime, entrypoint) = setup_crew_runtime_inner(
        loader,
        Some((ConversationSourceContext::root(snapshot), roles)),
    )?;
    Ok((
        lua,
        runtime,
        entrypoint.expect("snapshot runtime always has an entrypoint"),
    ))
}

fn setup_crew_runtime_inner(
    loader: &ProjectLoader,
    snapshot: Option<(ConversationSourceContext, FlowSourceRoles)>,
) -> Result<(mlua::Lua, Arc<Runtime>, Option<SnapshotLuaSource>)> {
    let lib_dirs = if snapshot.is_none() {
        vec![loader.project_dir().join("_lib")]
    } else {
        Vec::new()
    };
    let execution_policy = RuntimeExecutionPolicy::capture();
    let lua = create_crew_lua_with_policy(lib_dirs, execution_policy.lua_vm_policy()?)
        .map_err(IronCrewError::Lua)?;
    if let Some((context, _)) = &snapshot {
        lua.set_app_data(crate::lua::bootstrap::HttpConversationBootstrap);
        crate::lua::snapshot_require::install_snapshot_require(&lua, context.clone())
            .map_err(IronCrewError::Lua)?;
    }

    // Optionally load config.lua and store the resulting table as a Lua global.
    // Crew.new() will shallow-merge missing keys from this table at call time.
    let snapshot_config = snapshot
        .as_ref()
        .and_then(|(_, roles)| roles.config.as_ref());
    let live_config = snapshot
        .is_none()
        .then(|| loader.config_lua_path())
        .flatten();
    if snapshot_config.is_some() || live_config.is_some() {
        let cfg_path = snapshot_config
            .map(|source| source.relative_path().to_path_buf())
            .or(live_config)
            .expect("checked config source");
        let content = match snapshot_config {
            Some(source) => source.shared_source(),
            None => Arc::from(crate::lua::source::read_lua_source(&cfg_path)?),
        };
        let table: mlua::Table = {
            let _execution = LuaExecutionGuard::begin(&lua).map_err(IronCrewError::Lua)?;
            lua.load(content.as_ref())
                .set_name(format!("config:{}", cfg_path.display()))
                .eval()
                .map_err(|e| {
                    IronCrewError::Validation(format!(
                        "config.lua at {} must return a table: {}",
                        cfg_path.display(),
                        e
                    ))
                })?
        };
        lua.globals()
            .set("__ironcrew_config_defaults", table)
            .map_err(IronCrewError::Lua)?;
        tracing::info!("Loaded config.lua from {}", cfg_path.display());
    }

    // postgres.* capability: live database when IRONCREW_APP_DATABASE_URL is
    // set (feature-gated), fail-closed stub otherwise so a flow calling
    // postgres.* always gets a diagnosable error instead of a nil index.
    #[cfg(feature = "postgres")]
    {
        use crate::engine::app_db::{AppDb, operations, policy::AppDbPolicy};
        let app_db_url = std::env::var("IRONCREW_APP_DATABASE_URL")
            .ok()
            .filter(|value| !value.trim().is_empty());
        if let Some(url) = app_db_url {
            let app_policy = AppDbPolicy::capture()?;
            let sources = match &snapshot {
                Some((context, _)) => context
                    .snapshot
                    .sql_sources()
                    .into_iter()
                    .map(|(name, source)| (name, source.to_string()))
                    .collect(),
                None => operations::read_sql_dir(loader.project_dir(), &app_policy)?,
            };
            let registry = operations::OperationRegistry::from_sources(sources, &app_policy)?;
            let app_db = std::sync::Arc::new(AppDb::new(url, app_policy, registry));
            lua.set_app_data(crate::engine::conversation_definition::AppDbFingerprint(
                app_db.definition(),
            ));
            crate::lua::postgres::register_postgres(&lua, app_db).map_err(IronCrewError::Lua)?;
        } else {
            crate::lua::postgres::register_postgres_stub(
                &lua,
                crate::lua::postgres::STUB_UNCONFIGURED,
            )
            .map_err(IronCrewError::Lua)?;
        }
    }
    #[cfg(not(feature = "postgres"))]
    {
        if std::env::var("IRONCREW_APP_DATABASE_URL").is_ok_and(|value| !value.trim().is_empty()) {
            return Err(IronCrewError::Validation(
                "IRONCREW_APP_DATABASE_URL is set but this binary was built without the 'postgres' feature".into(),
            ));
        }
        crate::lua::postgres::register_postgres_stub(&lua, crate::lua::postgres::STUB_NO_FEATURE)
            .map_err(IronCrewError::Lua)?;
    }

    // Register globals
    register_agent_constructor(&lua).map_err(IronCrewError::Lua)?;

    // Create provider
    let api_key = std::env::var("OPENAI_API_KEY").unwrap_or_default();
    let base_url = std::env::var("OPENAI_BASE_URL").ok();
    if let Some(endpoint) = base_url.as_deref() {
        crate::engine::conversation_provider::validate_provider_endpoint(endpoint)?;
    }
    let provider = Box::new(OpenAiProvider::new(api_key, base_url));

    // Load declarative agents from agents/ directory
    let preloaded_agents = if let Some((_, roles)) = &snapshot {
        crate::lua::parsers::load_agents_from_snapshot(&roles.agents)?
    } else {
        load_agents_from_files(loader.agent_files())?
    };
    tracing::info!("Loaded {} agent(s) from files", preloaded_agents.len());

    // Load Lua tool definitions
    let tool_defs = if let Some((_, roles)) = &snapshot {
        crate::lua::parsers::load_tool_defs_from_snapshot(&roles.tools)?
    } else {
        load_tool_defs_from_files(loader.tool_files())?
    };

    // Create runtime with built-in + Lua tools
    let conversation_source = snapshot.as_ref().map(|(context, _)| context.clone());
    let mut runtime = Runtime::new_with_conversation_source_and_execution_policy(
        provider,
        Some(loader.project_dir()),
        conversation_source.clone(),
        execution_policy,
    );
    runtime.register_lua_tools(tool_defs)?;
    let runtime = Arc::new(runtime);
    // Propagate the weak self-ref into every registered LuaScriptTool so
    // sandbox-level `run_flow` can reach the tool registry without a
    // reference cycle.
    runtime.set_self_ref(Arc::downgrade(&runtime));

    // Seed sub-flow app-data on the top-level Lua VM. Sub-VMs created by
    // `invoke_subflow` inherit these via explicit set_app_data calls; the
    // top-level seeding here is what lets `run_flow` work from crew.lua
    // and from LuaScriptTool-hosted scripts.
    let project_dir_arc = Arc::new(loader.project_dir().to_path_buf());
    lua.set_app_data(runtime.clone());
    lua.set_app_data(project_dir_arc.clone());
    lua.set_app_data(crate::lua::subflow::SubflowDepth(0));
    if let Some(context) = conversation_source {
        lua.set_app_data(context);
    }

    // Register Crew.new() with preloaded agents auto-injected
    register_crew_constructor(
        &lua,
        runtime.clone(),
        preloaded_agents,
        loader.project_dir().to_path_buf(),
    )
    .map_err(IronCrewError::Lua)?;

    // Default mode is "run" — callers driving the VM in chat mode (the
    // `ironcrew chat` CLI command and the HTTP `start` handler) overwrite
    // this before executing the entrypoint.
    set_ironcrew_mode(&lua, "run").map_err(IronCrewError::Lua)?;

    let entrypoint = snapshot.map(|(_, roles)| roles.entrypoint);
    Ok((lua, runtime, entrypoint))
}

#[cfg(all(test, unix))]
mod snapshot_tests;
