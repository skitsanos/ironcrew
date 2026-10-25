use clap::{Args, Subcommand};

use crate::utils::error::{IronCrewError, Result};

#[derive(Args)]
pub struct HistoryArgs {
    /// Table prefix (otherwise IRONCREW_PG_TABLE_PREFIX); DATABASE_URL must be explicit in process env
    #[arg(long, global = true)]
    table_prefix: Option<String>,
    #[command(subcommand)]
    command: HistoryCommand,
}

#[derive(Subcommand)]
enum HistoryCommand {
    /// List PostgreSQL history metadata without runtime startup
    List {
        #[arg(long)]
        flow: Option<String>,
        #[arg(long, default_value = "50")]
        limit: usize,
        #[arg(long, default_value = "0")]
        offset: usize,
    },
    /// Read original fields with unknown legacy usage; output may contain private task content
    Inspect {
        run_id: String,
        /// Exact stored flow scope, including an explicitly empty scope; no directory lookup
        #[arg(long)]
        flow: Option<String>,
    },
}

#[cfg(feature = "postgres")]
pub async fn execute(args: HistoryArgs) -> Result<()> {
    use crate::history_reader::PostgresHistoryReader;
    let url = std::env::var("DATABASE_URL")
        .ok()
        .filter(|url| !url.trim().is_empty())
        .ok_or_else(|| {
            IronCrewError::Validation("History reader requires explicit DATABASE_URL".into())
        })?;
    let prefix = args
        .table_prefix
        .or_else(|| std::env::var("IRONCREW_PG_TABLE_PREFIX").ok())
        .unwrap_or_default();
    let mut reader = PostgresHistoryReader::connect(&url, &prefix).await?;
    let output = match args.command {
        HistoryCommand::List {
            flow,
            limit,
            offset,
        } => serde_json::to_string_pretty(&reader.list(flow.as_deref(), limit, offset).await?),
        HistoryCommand::Inspect { run_id, flow } => {
            serde_json::to_string_pretty(&reader.inspect(&run_id, flow.as_deref()).await?)
        }
    }
    .map_err(|_| IronCrewError::Validation("History view serialization failed".into()))?;
    println!("{output}");
    Ok(())
}

#[cfg(not(feature = "postgres"))]
pub async fn execute(_args: HistoryArgs) -> Result<()> {
    Err(IronCrewError::Validation(
        "History reader requires the postgres feature".into(),
    ))
}
