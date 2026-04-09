use async_trait::async_trait;

use crate::engine::run_history::{ListRunsFilter, RunRecord, RunSummary};
use crate::utils::error::Result;

/// Pluggable storage backend for run records.
#[async_trait]
pub trait StateStore: Send + Sync {
    async fn save_run(&self, record: &RunRecord) -> Result<String>;
    async fn get_run(&self, run_id: &str) -> Result<RunRecord>;

    /// Legacy full-record list. **Deprecated** — use `list_runs_summary` instead.
    /// Kept for backward compatibility with the CLI `clean` command and existing
    /// tests. Will be removed in v3.0.0.
    async fn list_runs(&self, status_filter: Option<&str>) -> Result<Vec<RunRecord>>;

    /// Paginated, metadata-only list view. Returns summaries without
    /// `task_results`, so clients can list hundreds of runs cheaply and fetch
    /// full records on demand via `get_run`.
    ///
    /// `limit` caps the number of rows returned (0 = unlimited).
    /// `offset` skips the first N rows (0 = start from the newest).
    /// `filter` selects runs matching status, tag, and/or since timestamp.
    async fn list_runs_summary(
        &self,
        filter: &ListRunsFilter,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<RunSummary>>;

    /// Count of runs matching `filter`. Paired with `list_runs_summary` to
    /// provide `total` in paginated API responses.
    async fn count_runs(&self, filter: &ListRunsFilter) -> Result<u64>;

    async fn delete_run(&self, run_id: &str) -> Result<()>;
}

/// Create a StateStore based on environment configuration.
///
/// `IRONCREW_STORE=json` (default) — JSON files in the given directory
/// `IRONCREW_STORE=sqlite` — SQLite database
/// `IRONCREW_STORE=postgres` — PostgreSQL (requires `postgres` feature)
/// `IRONCREW_STORE_PATH=<path>` — path for SQLite db (default: `<default_dir>/ironcrew.db`)
/// `DATABASE_URL=postgres://...` — PostgreSQL connection string
/// `IRONCREW_PG_TABLE_PREFIX=prefix_` — table prefix for shared databases
pub async fn create_store(default_dir: std::path::PathBuf) -> Result<Box<dyn StateStore>> {
    let store_type = std::env::var("IRONCREW_STORE").unwrap_or_else(|_| "json".into());

    match store_type.to_lowercase().as_str() {
        "sqlite" => {
            let db_path = std::env::var("IRONCREW_STORE_PATH")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|_| default_dir.join("ironcrew.db"));
            if let Some(parent) = db_path.parent() {
                std::fs::create_dir_all(parent)?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let _ =
                        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
                }
            }
            Ok(Box::new(super::sqlite_store::SqliteStore::new(db_path)?))
        }
        #[cfg(feature = "postgres")]
        "postgres" | "postgresql" => {
            let database_url = std::env::var("DATABASE_URL").map_err(|_| {
                crate::utils::error::IronCrewError::Validation(
                    "IRONCREW_STORE=postgres requires DATABASE_URL env var".into(),
                )
            })?;
            let table_prefix = std::env::var("IRONCREW_PG_TABLE_PREFIX").unwrap_or_default();
            let store =
                super::postgres_store::PostgresStore::new(&database_url, &table_prefix).await?;
            Ok(Box::new(store))
        }
        #[cfg(not(feature = "postgres"))]
        "postgres" | "postgresql" => Err(crate::utils::error::IronCrewError::Validation(
            "PostgreSQL backend requires building with --features postgres".into(),
        )),
        _ => {
            let runs_dir = default_dir.join("runs");
            Ok(Box::new(super::run_history::JsonFileStore::new(runs_dir)?))
        }
    }
}
