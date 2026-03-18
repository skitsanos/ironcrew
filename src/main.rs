mod api;
mod cli;
mod engine;
mod llm;
mod lua;
mod tools;
mod utils;

use std::path::PathBuf;

use clap::{Parser, Subcommand};

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
        Commands::Run { path } => cli::commands::cmd_run(&path).await,
        Commands::Validate { path } => cli::commands::cmd_validate(&path),
        Commands::List { path } => cli::commands::cmd_list(&path),
        Commands::Init { name } => cli::commands::cmd_init(&name),
        Commands::Nodes => cli::commands::cmd_nodes(),
        Commands::Inspect { run_id, project } => cli::history::cmd_inspect(&project, &run_id),
        Commands::Clean { project, keep, all } => cli::history::cmd_clean(&project, keep, all),
        Commands::Serve {
            host,
            port,
            flows_dir,
        } => cli::server::cmd_serve(&host, port, &flows_dir).await,
        Commands::Runs { status, project } => cli::history::cmd_runs(&project, status.as_deref()),
    };

    if let Err(e) = result {
        tracing::error!("{}", e);
        std::process::exit(1);
    }
}
