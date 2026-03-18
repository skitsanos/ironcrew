mod api;
mod engine;
mod llm;
mod lua;
mod tools;
mod utils;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use clap::{Parser, Subcommand};

use crate::engine::run_history::{RunHistory, RunStatus};
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
    /// Initialize a new IronCrew project
    Init {
        /// Project name (creates a directory with this name)
        #[arg(default_value = "my-crew")]
        name: String,
    },
    /// List all available built-in tools
    Nodes,
    /// Inspect a past run by ID
    Inspect {
        /// Run ID to inspect
        run_id: String,
        /// Project path (to find .ironcrew/runs/)
        #[arg(short, long, default_value = ".")]
        project: PathBuf,
    },
    /// Clean up old run history files
    Clean {
        /// Project path
        #[arg(short, long, default_value = ".")]
        project: PathBuf,
        /// Keep only the last N runs (default: 10)
        #[arg(short, long, default_value = "10")]
        keep: usize,
        /// Remove ALL run history
        #[arg(long)]
        all: bool,
    },
    /// Start the REST API server
    Serve {
        /// Host to bind to
        #[arg(long, default_value = "127.0.0.1")]
        host: String,
        /// Port to bind to
        #[arg(long, default_value = "3000")]
        port: u16,
        /// Directory containing crew flows
        #[arg(long, default_value = ".")]
        flows_dir: PathBuf,
    },
    /// List past runs
    Runs {
        /// Filter by status: success, partial_failure, failed
        #[arg(short, long)]
        status: Option<String>,
        /// Project path (to find .ironcrew/runs/)
        #[arg(short, long, default_value = ".")]
        project: PathBuf,
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
        Commands::Init { name } => cmd_init(&name),
        Commands::Nodes => cmd_nodes(),
        Commands::Inspect { run_id, project } => cmd_inspect(&project, &run_id),
        Commands::Clean { project, keep, all } => cmd_clean(&project, keep, all),
        Commands::Serve {
            host,
            port,
            flows_dir,
        } => cmd_serve(&host, port, &flows_dir).await,
        Commands::Runs { status, project } => cmd_runs(&project, status.as_deref()),
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

async fn cmd_serve(host: &str, port: u16, flows_dir: &Path) -> Result<()> {
    use tower_http::cors::CorsLayer;

    // Load .env from CWD
    dotenvy::dotenv().ok();

    let flows_dir = std::fs::canonicalize(flows_dir).unwrap_or_else(|_| flows_dir.to_path_buf());

    let state = Arc::new(api::AppState {
        flows_dir: flows_dir.clone(),
    });

    let app = api::create_router(state).layer(CorsLayer::permissive());

    let addr = format!("{}:{}", host, port);
    let listener = tokio::net::TcpListener::bind(&addr).await.map_err(|e| {
        IronCrewError::Validation(format!("Failed to bind to {}: {}", addr, e))
    })?;

    println!("IronCrew API server v{}", env!("CARGO_PKG_VERSION"));
    println!("Listening on http://{}", addr);
    println!("Flows directory: {}", flows_dir.display());
    println!();
    println!("Endpoints:");
    println!("  GET    /health                       - Health check");
    println!("  POST   /flows/{{flow}}/run             - Run a crew");
    println!("  GET    /flows/{{flow}}/runs            - List runs for a flow");
    println!("  GET    /flows/{{flow}}/runs/{{id}}       - Get run details");
    println!("  DELETE /flows/{{flow}}/runs/{{id}}       - Delete a run");
    println!("  GET    /flows/{{flow}}/validate         - Validate a flow");
    println!("  GET    /flows/{{flow}}/agents           - List agents in a flow");
    println!("  GET    /nodes                         - List built-in tools");

    axum::serve(listener, app).await.map_err(|e| {
        IronCrewError::Validation(format!("Server error: {}", e))
    })?;

    Ok(())
}

fn cmd_validate(path: &Path) -> Result<()> {
    let loader = load_project(path)?;
    let lua = create_crew_lua().map_err(IronCrewError::Lua)?;

    println!("Validating project: {}", loader.project_dir().display());
    println!();

    // 1. Validate agent files
    let agents = load_agents_from_files(&lua, loader.agent_files()).unwrap_or_default();
    println!("Agents ({}):", agents.len());
    for agent in &agents {
        let mut details = vec![];
        if !agent.capabilities.is_empty() {
            details.push(format!("capabilities: [{}]", agent.capabilities.join(", ")));
        }
        if !agent.tools.is_empty() {
            details.push(format!("tools: [{}]", agent.tools.join(", ")));
        }
        if let Some(ref model) = agent.model {
            details.push(format!("model: {}", model));
        }
        if agent.response_format.is_some() {
            details.push("response_format: set".into());
        }
        let detail_str = if details.is_empty() {
            String::new()
        } else {
            format!(" ({})", details.join(", "))
        };
        println!("  \u{2713} {}{}", agent.name, detail_str);
    }
    if agents.is_empty() {
        println!("  (none -- agents will be defined inline in crew.lua)");
    }
    println!();

    // 2. Validate tool files
    let tool_defs = load_tool_defs_from_files(&lua, loader.tool_files()).unwrap_or_default();
    let known_tools: Vec<String> = vec![
        "file_read",
        "file_write",
        "web_scrape",
        "shell",
        "http_request",
        "hash",
        "template_render",
    ]
    .into_iter()
    .map(String::from)
    .chain(tool_defs.iter().map(|t| t.name.clone()))
    .collect();

    println!("Tools ({} built-in + {} custom):", 7, tool_defs.len());
    println!("  Built-in: file_read, file_write, web_scrape, shell, http_request, hash, template_render");
    for tool in &tool_defs {
        println!(
            "  \u{2713} {} (custom, from {})",
            tool.name,
            tool.source_path.display()
        );
    }
    println!();

    // 3. Validate entrypoint syntax
    if let Some(entrypoint) = loader.entrypoint() {
        let script = std::fs::read_to_string(entrypoint)?;
        lua.load(&script).into_function().map_err(|e| {
            IronCrewError::Validation(format!("Syntax error in {}: {}", entrypoint.display(), e))
        })?;
        println!("Entrypoint: \u{2713} {} (syntax valid)", entrypoint.display());
    }
    println!();

    // 4. Reference integrity: agent tool references
    let mut issues = 0;
    for agent in &agents {
        for tool_name in &agent.tools {
            if !known_tools.contains(tool_name) {
                println!(
                    "  \u{2717} Agent '{}' references unknown tool '{}'",
                    agent.name, tool_name
                );
                issues += 1;
            }
        }
    }

    if issues == 0 {
        println!("Reference integrity: \u{2713} all references valid");
    }
    println!();

    // 5. Summary
    if issues > 0 {
        println!("Validation FAILED with {} issue(s).", issues);
        Err(IronCrewError::Validation(format!(
            "{} validation issue(s) found",
            issues
        )))
    } else {
        println!("Validation PASSED.");
        println!();
        println!("Note: Task dependencies and execution order are validated at runtime");
        println!("      when crew.lua is executed (tasks are defined programmatically).");
        Ok(())
    }
}

fn cmd_init(name: &str) -> Result<()> {
    let project_dir = Path::new(name);

    if project_dir.exists() {
        return Err(IronCrewError::Validation(format!(
            "Directory '{}' already exists",
            name
        )));
    }

    // Create directory structure
    std::fs::create_dir_all(project_dir.join("agents"))?;
    std::fs::create_dir_all(project_dir.join("tools"))?;

    // Write .env template
    std::fs::write(
        project_dir.join(".env"),
        "# IronCrew Environment Configuration\n\
         OPENAI_API_KEY=your-api-key-here\n\
         OPENAI_BASE_URL=https://api.openai.com/v1\n\
         OPENAI_MODEL=gpt-4o-mini\n\
         IRONCREW_LOG=info\n",
    )?;

    // Write .gitignore
    std::fs::write(
        project_dir.join(".gitignore"),
        "/output\n\
         .env\n\
         .DS_Store\n\
         .ironcrew/\n",
    )?;

    // Write sample agent
    std::fs::write(
        project_dir.join("agents/assistant.lua"),
        r#"return {
    name = "assistant",
    goal = "Help with tasks by providing clear, accurate responses",
    capabilities = {"general", "analysis", "writing"},
    temperature = 0.7,
}
"#,
    )?;

    // Write crew.lua entrypoint
    std::fs::write(
        project_dir.join("crew.lua"),
        format!(
            r#"--[[
    {name} - IronCrew Project

    Run with: ironcrew run .
    Validate with: ironcrew validate .
]]

local crew = Crew.new({{
    goal = "Your crew goal here",
    provider = "openai",
    model = env("OPENAI_MODEL") or "gpt-4o-mini",
    base_url = env("OPENAI_BASE_URL"),
}})

-- Add tasks
crew:add_task({{
    name = "hello",
    description = "Say hello and introduce yourself briefly",
    expected_output = "A friendly greeting",
}})

-- Run the crew
local results = crew:run()

-- Display results
for _, result in ipairs(results) do
    if result.success then
        print("=== " .. result.task .. " (" .. result.duration_ms .. "ms) ===")
        print(result.output)
    else
        print("FAILED: " .. result.task .. " - " .. result.output)
    end
    print()
end
"#,
            name = name
        ),
    )?;

    println!("Created new IronCrew project: {}", name);
    println!();
    println!("  {}/", name);
    println!("  ├── .env              # API keys and config");
    println!("  ├── .gitignore");
    println!("  ├── agents/");
    println!("  │   └── assistant.lua # Sample agent");
    println!("  ├── tools/            # Custom tools (empty)");
    println!("  └── crew.lua          # Entrypoint");
    println!();
    println!("Next steps:");
    println!("  1. cd {}", name);
    println!("  2. Edit .env with your API key");
    println!("  3. ironcrew run .");

    Ok(())
}

fn cmd_nodes() -> Result<()> {
    // Create a temporary registry to get all built-in tools
    let mut registry = crate::tools::registry::ToolRegistry::new();

    // Register all built-in tools
    registry.register(Box::new(crate::tools::file_read::FileReadTool::new(None)));
    registry.register(Box::new(crate::tools::file_write::FileWriteTool::new(
        None, None,
    )));
    registry.register(Box::new(crate::tools::web_scrape::WebScrapeTool::new(
        None,
    )));
    registry.register(Box::new(crate::tools::shell::ShellTool::new()));
    registry.register(Box::new(
        crate::tools::http_request::HttpRequestTool::new(),
    ));
    registry.register(Box::new(crate::tools::hash::HashTool::new()));
    registry.register(Box::new(
        crate::tools::template_render::TemplateRenderTool::new(),
    ));

    println!("Built-in tools ({}):", registry.list().len());
    println!();

    // Get tools sorted by name
    let mut tools: Vec<(String, String)> = Vec::new();
    for name in registry.list() {
        if let Some(tool) = registry.get(&name) {
            tools.push((name, tool.description().to_string()));
        }
    }
    tools.sort_by(|a, b| a.0.cmp(&b.0));

    // Find the longest name for alignment
    let max_name_len = tools.iter().map(|(n, _)| n.len()).max().unwrap_or(0);

    for (name, description) in &tools {
        println!("  {:<width$}  {}", name, description, width = max_name_len);
    }

    println!();
    println!("Custom tools can be defined in tools/*.lua files in your project.");

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
        println!("  {} -- {}", agent.name, agent.goal);
        if !agent.capabilities.is_empty() {
            println!("    capabilities: [{}]", agent.capabilities.join(", "));
        }
        if !agent.tools.is_empty() {
            println!("    tools: [{}]", agent.tools.join(", "));
        }
        if let Some(ref model) = agent.model {
            println!("    model: {}", model);
        }
        if let Some(temp) = agent.temperature {
            println!("    temperature: {}", temp);
        }
        if agent.response_format.is_some() {
            println!("    response_format: configured");
        }
    }
    println!();

    // List tool files
    let tool_defs = load_tool_defs_from_files(&lua, loader.tool_files()).unwrap_or_default();
    println!("Custom tools ({}):", tool_defs.len());
    for tool in &tool_defs {
        println!(
            "  {} -- from {}",
            tool.name,
            tool.source_path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
        );
    }
    if tool_defs.is_empty() {
        println!("  (none)");
    }
    println!();

    println!("Built-in tools (7): file_read, file_write, web_scrape, shell, http_request, hash, template_render");
    println!();

    // Entrypoint
    if let Some(ep) = loader.entrypoint() {
        println!("Entrypoint: {}", ep.display());
    }

    Ok(())
}

fn cmd_clean(project: &Path, keep: usize, all: bool) -> Result<()> {
    let store_dir = project.join(".ironcrew").join("runs");

    if !store_dir.exists() {
        println!("No run history found.");
        return Ok(());
    }

    let history = RunHistory::new(store_dir)?;
    let runs = history.list(None)?;

    if runs.is_empty() {
        println!("No runs to clean.");
        return Ok(());
    }

    if all {
        // Delete everything
        let count = runs.len();
        for run in &runs {
            history.delete(&run.run_id)?;
        }
        println!("Deleted all {} run(s).", count);

        // Also clean up memory file if it exists
        let memory_file = project.join(".ironcrew").join("memory.json");
        if memory_file.exists() {
            std::fs::remove_file(&memory_file)?;
            println!("Deleted memory store.");
        }
    } else {
        // Keep the last N, delete the rest
        // runs are already sorted newest-first from history.list()
        if runs.len() <= keep {
            println!(
                "Only {} run(s) found, nothing to clean (keeping {}).",
                runs.len(),
                keep
            );
            return Ok(());
        }

        let to_delete = &runs[keep..];
        let count = to_delete.len();
        for run in to_delete {
            history.delete(&run.run_id)?;
        }
        println!("Deleted {} old run(s), kept {} most recent.", count, keep);
    }

    Ok(())
}

fn cmd_runs(project: &Path, status_filter: Option<&str>) -> Result<()> {
    let store_dir = project.join(".ironcrew").join("runs");
    let history = RunHistory::new(store_dir)?;
    let runs = history.list(status_filter)?;

    if runs.is_empty() {
        println!("No runs found.");
        return Ok(());
    }

    let header_started = "STARTED";
    println!(
        "{:<38} {:<16} {:<10} {:<10} {}",
        "RUN ID", "STATUS", "TASKS", "DURATION", header_started
    );
    println!("{}", "-".repeat(90));

    for run in &runs {
        let status_display = match run.status {
            RunStatus::Success => "success",
            RunStatus::PartialFailure => "partial",
            RunStatus::Failed => "failed",
        };
        println!(
            "{:<38} {:<16} {:<10} {:<10} {}",
            run.run_id,
            status_display,
            format!(
                "{}/{}",
                run.task_results.iter().filter(|r| r.success).count(),
                run.task_count
            ),
            format!("{}ms", run.duration_ms),
            if run.started_at.len() >= 19 {
                &run.started_at[..19]
            } else {
                &run.started_at
            },
        );
    }

    println!("\n{} run(s) total.", runs.len());
    Ok(())
}

fn cmd_inspect(project: &Path, run_id: &str) -> Result<()> {
    let store_dir = project.join(".ironcrew").join("runs");
    let history = RunHistory::new(store_dir)?;
    let record = history.get(run_id)?;

    println!("Run: {}", record.run_id);
    println!("Flow: {}", record.flow_name);
    println!("Status: {}", record.status);
    println!("Started: {}", record.started_at);
    println!("Finished: {}", record.finished_at);
    println!("Duration: {}ms", record.duration_ms);
    println!("Agents: {}", record.agent_count);
    println!(
        "Tasks: {}/{} succeeded",
        record.task_results.iter().filter(|r| r.success).count(),
        record.task_count
    );
    println!();

    for result in &record.task_results {
        let status = if result.success { "OK" } else { "FAIL" };
        let agent = if result.agent.is_empty() {
            "(none)"
        } else {
            &result.agent
        };
        println!(
            "[{}] {} (by {}, {}ms)",
            status, result.task, agent, result.duration_ms
        );
        // Truncate long output
        let output = if result.output.len() > 200 {
            format!("{}...", &result.output[..200])
        } else {
            result.output.clone()
        };
        println!("  {}", output);
        println!();
    }

    Ok(())
}
