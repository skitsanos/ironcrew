mod engine;
mod llm;
mod lua;
mod tools;
mod utils;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use clap::{Parser, Subcommand};

use crate::llm::openai::OpenAiProvider;
use crate::lua::api::{
    load_agents_from_files, load_tool_defs_from_files, register_agent_constructor,
    register_crew_constructor,
};
use crate::lua::loader::ProjectLoader;
use crate::lua::sandbox::create_crew_lua;
use crate::utils::error::{IronCrewError, Result};

#[derive(Parser)]
#[command(name = "ironcrew", version, about = "Lua-scripted AI agent crew runner")]
struct Cli {
    #[command(subcommand)]
    command: Commands,

    /// Enable verbose output
    #[arg(short, long, global = true)]
    verbose: bool,
}

#[derive(Subcommand)]
enum Commands {
    /// Run a crew from a directory or Lua file
    Run {
        /// Path to project directory or crew.lua file
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// Validate Lua files without executing
    Validate {
        /// Path to project directory or crew.lua file
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// List discovered agents, tools, and tasks
    List {
        /// Path to project directory or crew.lua file
        #[arg(default_value = ".")]
        path: PathBuf,
    },
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    utils::logger::init(cli.verbose);

    let result = match cli.command {
        Commands::Run { path } => cmd_run(&path).await,
        Commands::Validate { path } => cmd_validate(&path),
        Commands::List { path } => cmd_list(&path),
    };

    if let Err(e) = result {
        tracing::error!("{}", e);
        std::process::exit(1);
    }
}

fn load_project(path: &Path) -> Result<ProjectLoader> {
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

async fn cmd_run(path: &Path) -> Result<()> {
    let loader = load_project(path)?;
    let lua = create_crew_lua().map_err(IronCrewError::Lua)?;

    // Register globals
    register_agent_constructor(&lua).map_err(IronCrewError::Lua)?;

    // Create provider
    let api_key = std::env::var("OPENAI_API_KEY").map_err(|_| {
        IronCrewError::Validation("OPENAI_API_KEY environment variable not set".into())
    })?;
    let base_url = std::env::var("OPENAI_BASE_URL").ok();
    let provider = Box::new(OpenAiProvider::new(api_key, base_url));

    // Load declarative agents from agents/ directory
    let preloaded_agents = load_agents_from_files(&lua, loader.agent_files())?;
    tracing::info!("Loaded {} agent(s) from files", preloaded_agents.len());

    // Load Lua tool definitions
    let tool_defs = load_tool_defs_from_files(&lua, loader.tool_files())?;

    // Create runtime (single construction) with built-in + Lua tools
    let mut runtime = engine::runtime::Runtime::new(provider, Some(loader.project_dir()));
    runtime.register_lua_tools(tool_defs);
    let runtime = Arc::new(runtime);

    // Register Crew.new() with preloaded agents auto-injected
    register_crew_constructor(
        &lua,
        runtime.clone(),
        preloaded_agents,
        loader.project_dir().to_path_buf(),
    )
    .map_err(IronCrewError::Lua)?;

    // Execute entrypoint
    let entrypoint = loader
        .entrypoint()
        .ok_or_else(|| IronCrewError::Validation("No entrypoint found".into()))?;
    let script = std::fs::read_to_string(entrypoint)?;

    tracing::info!("Running {}", entrypoint.display());

    lua.load(&script)
        .exec_async()
        .await
        .map_err(IronCrewError::Lua)?;

    Ok(())
}

fn cmd_validate(path: &Path) -> Result<()> {
    let loader = load_project(path)?;
    let lua = create_crew_lua().map_err(IronCrewError::Lua)?;

    // 1. Validate agent files (Lua syntax + schema: name and goal required)
    let agents = load_agents_from_files(&lua, loader.agent_files())?;
    println!("Agents: {} valid", agents.len());

    // 2. Validate tool files (Lua syntax + schema: name, description, parameters, execute required)
    let tool_defs = load_tool_defs_from_files(&lua, loader.tool_files())?;
    println!("Tools: {} valid", tool_defs.len());

    // 3. Validate entrypoint Lua syntax
    if let Some(entrypoint) = loader.entrypoint() {
        let script = std::fs::read_to_string(entrypoint)?;
        // Check Lua syntax only (load but don't exec, since exec would run the crew)
        lua.load(&script).into_function().map_err(|e| {
            IronCrewError::Validation(format!("Syntax error in {}: {}", entrypoint.display(), e))
        })?;
        println!("Entrypoint: {} valid", entrypoint.display());
    }

    // 4. Validate agent tool references resolve to known tools
    let known_tools: Vec<String> = vec![
        "file_read".into(),
        "file_write".into(),
        "web_scrape".into(),
        "shell".into(),
    ]
    .into_iter()
    .chain(tool_defs.iter().map(|t| t.name.clone()))
    .collect();
    for agent in &agents {
        for tool_name in &agent.tools {
            if !known_tools.contains(tool_name) {
                return Err(IronCrewError::Validation(format!(
                    "Agent '{}' references unknown tool '{}'",
                    agent.name, tool_name
                )));
            }
        }
    }
    println!("Reference integrity: valid");

    // Note: task dependency graph validation happens at runtime (tasks are defined in crew.lua)
    // ResponseFormat validation happens during agent loading (parse_response_format)

    println!("Validation passed.");
    Ok(())
}

fn cmd_list(path: &Path) -> Result<()> {
    let loader = load_project(path)?;
    let lua = create_crew_lua().map_err(IronCrewError::Lua)?;

    println!("Project: {}", loader.project_dir().display());
    println!();

    // List agents
    let agents = load_agents_from_files(&lua, loader.agent_files()).unwrap_or_default();
    println!("Agents ({}):", agents.len());
    for agent in &agents {
        println!(
            "  - {} (capabilities: [{}], tools: [{}])",
            agent.name,
            agent.capabilities.join(", "),
            agent.tools.join(", ")
        );
    }
    println!();

    // List tool files
    println!("Tool files ({}):", loader.tool_files().len());
    for tool_file in loader.tool_files() {
        println!("  - {}", tool_file.display());
    }
    println!();

    // Entrypoint
    if let Some(ep) = loader.entrypoint() {
        println!("Entrypoint: {}", ep.display());
    }

    Ok(())
}
