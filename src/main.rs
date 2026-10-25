mod api;
mod cli;
mod engine;
mod llm;
mod lua;
#[cfg(feature = "mcp")]
mod mcp;
mod metrics;
mod tools;
mod utils;
use ironcrew::usage;

use std::path::PathBuf;

use clap::{Parser, Subcommand};
#[path = "main/command_environment.rs"]
mod command_environment;
#[path = "main/serve_address.rs"]
mod serve_address;
use serve_address::{ServeEnvironment, resolve_serve_address};

#[derive(Parser)]
#[command(
    name = "ironcrew",
    version,
    about = "Lua-scripted AI agent crew runner"
)]
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
        /// JSON input passed as the `input` global in Lua
        #[arg(short, long)]
        input: Option<String>,
        /// Output structured JSON instead of Lua print() statements
        #[arg(long)]
        json: bool,
        /// Tag this run with a label (repeatable: --tag v2 --tag experiment)
        #[arg(short, long)]
        tag: Vec<String>,
    },
    /// Validate declarations, or evaluate construction without external effects
    Validate {
        /// Path to project directory or crew.lua file
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Evaluate construction; exit 3 if execution or external state is required
        #[arg(long)]
        evaluate: bool,
        #[arg(long, hide = true, requires = "evaluate")]
        construction_worker: bool,
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
    /// Read PostgreSQL history without runtime startup, writes, or implicit .env loading
    History(ironcrew::history_cli::HistoryArgs),
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
        /// Host to bind to (IRONCREW_HOST; defaults to 0.0.0.0 when PORT is set,
        /// otherwise 127.0.0.1)
        #[arg(long)]
        host: Option<String>,
        /// Port to bind to (IRONCREW_PORT, then platform PORT, then 3000)
        #[arg(long)]
        port: Option<u16>,
        /// Directory containing crew flows
        #[arg(long, default_value = ".")]
        flows_dir: PathBuf,
    },
    /// Lint and check Lua crew files for common issues
    Fmt {
        /// Path to project directory or crew.lua file
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// Check environment, API keys, and project health
    Doctor {
        /// Project path to diagnose
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// Export a flow as a standalone package for sharing
    Export {
        /// Path to project directory
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Output directory path (default: <project-name>-export)
        #[arg(short, long)]
        output: Option<PathBuf>,
    },
    /// Generate a DAG visualization HTML file
    Graph {
        /// Path to project directory
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Output HTML file path (default: <project>/graph.html)
        #[arg(short, long)]
        output: Option<PathBuf>,
    },
    /// Start an interactive chat REPL against a conversational agent
    Chat {
        /// Path to project directory or crew.lua file
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Agent name to converse with (must be declared in crew.lua)
        #[arg(long)]
        agent: Option<String>,
        /// Stable session id (enables cross-run persistence)
        #[arg(long)]
        id: Option<String>,
    },
    /// List past runs
    Runs {
        /// Filter by status: success, partial_failure, failed, aborted,
        /// timed_out, running, waiting_for_input, abandoned
        #[arg(short, long)]
        status: Option<String>,
        /// Filter by tag
        #[arg(short, long)]
        tag: Option<String>,
        /// Only show runs started at or after this RFC3339 timestamp
        #[arg(long)]
        since: Option<String>,
        /// Maximum number of runs to return (default 20)
        #[arg(short, long, default_value_t = 20)]
        limit: usize,
        /// Skip the first N runs (for pagination)
        #[arg(short, long, default_value_t = 0)]
        offset: usize,
        /// Project path (to find .ironcrew/runs/)
        #[arg(short, long, default_value = ".")]
        project: PathBuf,
    },
}

fn main() {
    let cli = Cli::parse();

    // Load `.env` BEFORE the async runtime starts. `dotenvy` mutates the
    // environment via `std::env::set_var`, which is only sound while the process
    // is single-threaded — doing it here (before any Tokio worker thread exists)
    // avoids the data race that per-request loading caused. Loading before the
    // logger also lets `IRONCREW_LOG` be set from `.env`.
    command_environment::load_command_environment(&cli.command);
    utils::logger::init(cli.verbose);

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to build Tokio runtime");

    let result = runtime.block_on(async {
        match cli.command {
            Commands::Run {
                path,
                input,
                json,
                tag,
            } => cli::commands::cmd_run(&path, input.as_deref(), json, tag).await,
            Commands::Validate {
                path,
                evaluate,
                construction_worker,
            } => {
                if evaluate {
                    cli::validation::cmd_evaluate(&path, construction_worker).await
                } else {
                    cli::commands::cmd_validate(&path)
                }
            }
            Commands::List { path } => cli::commands::cmd_list(&path),
            Commands::Init { name } => cli::commands::cmd_init(&name),
            Commands::Nodes => cli::commands::cmd_nodes(),
            Commands::History(args) => ironcrew::history_cli::execute(args)
                .await
                .map_err(|error| utils::error::IronCrewError::Validation(error.to_string())),
            Commands::Inspect { run_id, project } => {
                cli::history::cmd_inspect(&project, &run_id).await
            }
            Commands::Clean { project, keep, all } => {
                cli::history::cmd_clean(&project, keep, all).await
            }
            Commands::Serve {
                host,
                port,
                flows_dir,
            } => {
                let (host, port) =
                    resolve_serve_address(host, port, ServeEnvironment::from_process()?)?;
                cli::server::cmd_serve(&host, port, &flows_dir).await
            }
            Commands::Fmt { path } => cli::commands::cmd_fmt(&path),
            Commands::Doctor { path } => cli::commands::cmd_doctor(&path),
            Commands::Export { path, output } => {
                cli::commands::cmd_export(&path, output.as_deref())
            }
            Commands::Graph { path, output } => cli::graph::cmd_graph(&path, output.as_deref()),
            Commands::Chat { path, agent, id } => cli::chat::cmd_chat(&path, agent, id).await,
            Commands::Runs {
                status,
                tag,
                since,
                limit,
                offset,
                project,
            } => {
                cli::history::cmd_runs(
                    &project,
                    status.as_deref(),
                    tag.as_deref(),
                    since.as_deref(),
                    limit,
                    offset,
                )
                .await
            }
        }
    });

    if let Err(e) = result {
        tracing::error!("{}", e);
        std::process::exit(
            if matches!(e, utils::error::IronCrewError::ValidationIncomplete(_)) {
                3
            } else {
                1
            },
        );
    }
}

#[cfg(test)]
#[path = "main_tests.rs"]
mod tests;
