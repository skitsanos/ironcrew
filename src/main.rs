mod api;
mod cli;
mod engine;
mod llm;
mod lua;
#[cfg(feature = "mcp")]
mod mcp;
mod tools;
mod utils;

use std::{env, path::PathBuf};

use clap::{Parser, Subcommand};
use utils::error::{IronCrewError, Result};

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

#[derive(Debug, Default, PartialEq, Eq)]
struct ServeEnvironment {
    host: Option<String>,
    ironcrew_port: Option<String>,
    platform_port: Option<String>,
}

impl ServeEnvironment {
    fn from_process() -> Result<Self> {
        Ok(Self {
            host: read_optional_env("IRONCREW_HOST")?,
            ironcrew_port: read_optional_env("IRONCREW_PORT")?,
            platform_port: read_optional_env("PORT")?,
        })
    }
}

fn read_optional_env(name: &str) -> Result<Option<String>> {
    match env::var(name) {
        Ok(value) => Ok(Some(value)),
        Err(env::VarError::NotPresent) => Ok(None),
        Err(env::VarError::NotUnicode(_)) => Err(IronCrewError::Validation(format!(
            "{name} must contain valid UTF-8"
        ))),
    }
}

fn parse_port(name: &str, value: &str) -> Result<u16> {
    let port = value.parse::<u16>().map_err(|_| {
        IronCrewError::Validation(format!(
            "{name} must be an integer between 1 and 65535, got {value:?}"
        ))
    })?;
    if port == 0 {
        return Err(IronCrewError::Validation(format!(
            "{name} must be between 1 and 65535, got 0"
        )));
    }
    Ok(port)
}

/// Resolve server binding without making container-only defaults leak into the
/// local CLI. Explicit flags take precedence over IronCrew-specific variables,
/// which take precedence over Railway's conventional `PORT` variable.
fn resolve_serve_address(
    cli_host: Option<String>,
    cli_port: Option<u16>,
    environment: ServeEnvironment,
) -> Result<(String, u16)> {
    let platform_port_is_set = environment.platform_port.is_some();

    let host = cli_host.or(environment.host).unwrap_or_else(|| {
        if platform_port_is_set {
            "0.0.0.0".to_owned()
        } else {
            "127.0.0.1".to_owned()
        }
    });
    if host.trim().is_empty() {
        return Err(IronCrewError::Validation(
            "server host must not be empty".into(),
        ));
    }

    let port = match cli_port {
        Some(0) => {
            return Err(IronCrewError::Validation(
                "--port must be between 1 and 65535, got 0".into(),
            ));
        }
        Some(port) => port,
        None => match environment.ironcrew_port {
            Some(value) => parse_port("IRONCREW_PORT", &value)?,
            None => match environment.platform_port {
                Some(value) => parse_port("PORT", &value)?,
                None => 3000,
            },
        },
    };

    Ok((host, port))
}

/// The project/CWD path a command operates on, used to locate its `.env`.
/// `None` for commands that don't target a project (`init`, `nodes`, `serve` —
/// the server uses the CWD `.env` and process environment, never per-flow files).
fn command_path(command: &Commands) -> Option<&std::path::Path> {
    match command {
        Commands::Run { path, .. }
        | Commands::Validate { path }
        | Commands::List { path }
        | Commands::Fmt { path }
        | Commands::Doctor { path }
        | Commands::Export { path, .. }
        | Commands::Graph { path, .. }
        | Commands::Chat { path, .. } => Some(path),
        Commands::Inspect { project, .. }
        | Commands::Clean { project, .. }
        | Commands::Runs { project, .. } => Some(project),
        Commands::Init { .. } | Commands::Nodes | Commands::Serve { .. } => None,
    }
}

fn main() {
    let cli = Cli::parse();

    // Load `.env` BEFORE the async runtime starts. `dotenvy` mutates the
    // environment via `std::env::set_var`, which is only sound while the process
    // is single-threaded — doing it here (before any Tokio worker thread exists)
    // avoids the data race that per-request loading caused. Loading before the
    // logger also lets `IRONCREW_LOG` be set from `.env`.
    cli::project::load_dotenv(command_path(&cli.command));
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
            Commands::Validate { path } => cli::commands::cmd_validate(&path),
            Commands::List { path } => cli::commands::cmd_list(&path),
            Commands::Init { name } => cli::commands::cmd_init(&name),
            Commands::Nodes => cli::commands::cmd_nodes(),
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
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::{ServeEnvironment, resolve_serve_address};

    fn environment(
        host: Option<&str>,
        ironcrew_port: Option<&str>,
        platform_port: Option<&str>,
    ) -> ServeEnvironment {
        ServeEnvironment {
            host: host.map(str::to_owned),
            ironcrew_port: ironcrew_port.map(str::to_owned),
            platform_port: platform_port.map(str::to_owned),
        }
    }

    #[test]
    fn serve_defaults_remain_local() {
        let address = resolve_serve_address(None, None, environment(None, None, None)).unwrap();
        assert_eq!(address, ("127.0.0.1".to_owned(), 3000));
    }

    #[test]
    fn railway_port_binds_all_interfaces() {
        let address =
            resolve_serve_address(None, None, environment(None, None, Some("48123"))).unwrap();
        assert_eq!(address, ("0.0.0.0".to_owned(), 48123));
    }

    #[test]
    fn ironcrew_environment_takes_precedence_over_platform_port() {
        let address = resolve_serve_address(
            None,
            None,
            environment(Some("::"), Some("4100"), Some("48123")),
        )
        .unwrap();
        assert_eq!(address, ("::".to_owned(), 4100));
    }

    #[test]
    fn explicit_arguments_take_precedence_over_environment() {
        let address = resolve_serve_address(
            Some("127.0.0.2".to_owned()),
            Some(4200),
            environment(Some("::"), Some("4100"), Some("48123")),
        )
        .unwrap();
        assert_eq!(address, ("127.0.0.2".to_owned(), 4200));
    }

    #[test]
    fn invalid_ironcrew_port_fails_loudly() {
        let error = resolve_serve_address(
            None,
            None,
            environment(None, Some("not-a-port"), Some("48123")),
        )
        .unwrap_err();
        assert!(error.to_string().contains("IRONCREW_PORT"));
    }

    #[test]
    fn invalid_platform_port_fails_loudly() {
        let error =
            resolve_serve_address(None, None, environment(None, None, Some("70000"))).unwrap_err();
        assert!(error.to_string().contains("PORT"));
    }

    #[test]
    fn zero_port_is_rejected() {
        let error =
            resolve_serve_address(None, Some(0), environment(None, None, None)).unwrap_err();
        assert!(error.to_string().contains("--port"));
    }
}
