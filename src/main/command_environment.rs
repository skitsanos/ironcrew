use super::Commands;

/// Run before constructing Tokio workers: dotenv mutates process environment.
pub(super) fn load_command_environment(command: &Commands) {
    // Recovery uses explicit process settings, never an ancestor .env.
    if matches!(
        command,
        Commands::Validate { evaluate: true, .. } | Commands::History(_)
    ) {
        return;
    }
    crate::cli::project::load_dotenv(command_path(command));
}

/// The project/CWD path a command operates on, used to locate its `.env`.
/// `None` for commands that don't target a project (`init`, `nodes`, `serve` —
/// the server uses the CWD `.env` and process environment, never per-flow files).
fn command_path(command: &Commands) -> Option<&std::path::Path> {
    match command {
        Commands::Run { path, .. }
        | Commands::Validate { path, .. }
        | Commands::List { path }
        | Commands::Fmt { path }
        | Commands::Doctor { path }
        | Commands::Export { path, .. }
        | Commands::Graph { path, .. }
        | Commands::Chat { path, .. } => Some(path),
        Commands::Inspect { project, .. }
        | Commands::Clean { project, .. }
        | Commands::Runs { project, .. } => Some(project),
        Commands::Init { .. } | Commands::Nodes | Commands::Serve { .. } | Commands::History(_) => {
            None
        }
    }
}
