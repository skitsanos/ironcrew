use std::path::Path;
use std::sync::Arc;

use crate::engine::runtime::Runtime;
use crate::llm::openai::OpenAiProvider;
use crate::lua::api::{
    load_agents_from_files, load_tool_defs_from_files, register_agent_constructor,
    register_crew_constructor, set_ironcrew_mode,
};
use crate::lua::loader::ProjectLoader;
use crate::lua::sandbox::create_crew_lua;
use crate::utils::error::{IronCrewError, Result};

/// Load a project from a path (file or directory), handling .env loading.
pub fn load_project(path: &Path) -> Result<ProjectLoader> {
    // Load .env: check CWD first, then project directory
    dotenvy::dotenv().ok();

    let project_dir = if path.is_file() {
        path.parent().unwrap_or(Path::new("."))
    } else {
        path
    };

    // Project-level .env overrides CWD .env
    let env_file = project_dir.join(".env");
    if env_file.exists() {
        dotenvy::from_path(&env_file).ok();
    }

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
    let lua = create_crew_lua().map_err(IronCrewError::Lua)?;

    // Optionally load config.lua and store the resulting table as a Lua global.
    // Crew.new() will shallow-merge missing keys from this table at call time.
    if let Some(cfg_path) = loader.config_lua_path() {
        let content = std::fs::read_to_string(&cfg_path).map_err(IronCrewError::Io)?;
        let table: mlua::Table = lua
            .load(&content)
            .set_name(format!("config:{}", cfg_path.display()))
            .eval()
            .map_err(|e| {
                IronCrewError::Validation(format!(
                    "config.lua at {} must return a table: {}",
                    cfg_path.display(),
                    e
                ))
            })?;
        lua.globals()
            .set("__ironcrew_config_defaults", table)
            .map_err(IronCrewError::Lua)?;
        tracing::info!("Loaded config.lua from {}", cfg_path.display());
    }

    // Register globals
    register_agent_constructor(&lua).map_err(IronCrewError::Lua)?;

    // Create provider
    let api_key = std::env::var("OPENAI_API_KEY").unwrap_or_default();
    let base_url = std::env::var("OPENAI_BASE_URL").ok();
    let provider = Box::new(OpenAiProvider::new(api_key, base_url));

    // Load declarative agents from agents/ directory
    let preloaded_agents = load_agents_from_files(loader.agent_files())?;
    tracing::info!("Loaded {} agent(s) from files", preloaded_agents.len());

    // Load Lua tool definitions
    let tool_defs = load_tool_defs_from_files(loader.tool_files())?;

    // Create runtime with built-in + Lua tools
    let mut runtime = Runtime::new(provider, Some(loader.project_dir()));
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

    Ok((lua, runtime))
}
