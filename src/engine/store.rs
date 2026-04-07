use async_trait::async_trait;

use crate::engine::run_history::RunRecord;
use crate::utils::error::Result;

/// Pluggable storage backend for run records.
#[async_trait]
pub trait StateStore: Send + Sync {
    async fn save_run(&self, record: &RunRecord) -> Result<String>;
    async fn get_run(&self, run_id: &str) -> Result<RunRecord>;
    async fn list_runs(&self, status_filter: Option<&str>) -> Result<Vec<RunRecord>>;
    async fn delete_run(&self, run_id: &str) -> Result<()>;
}

/// Create a StateStore based on environment configuration.
///
/// `IRONCREW_STORE=json` (default) — JSON files in the given directory
/// `IRONCREW_STORE=sqlite` — SQLite database
/// `IRONCREW_STORE=postgres` — PostgreSQL (requires `postgres` feature)
/// `IRONCREW_STORE_PATH=<path>` — path for SQLite db (default: `<default_dir>/ironcrew.db`)
/// `DATABASE_URL=postgres://...` — PostgreSQL connection string
pub fn create_store(default_dir: std::path::PathBuf) -> Result<Box<dyn StateStore>> {
    let store_type = std::env::var("IRONCREW_STORE").unwrap_or_else(|_| "json".into());

    match store_type.to_lowercase().as_str() {
        "sqlite" => {
            let db_path = std::env::var("IRONCREW_STORE_PATH")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|_| default_dir.join("ironcrew.db"));
            if let Some(parent) = db_path.parent() {
                std::fs::create_dir_all(parent)?;
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
            // PostgresStore::new is async — use a blocking approach in this sync factory
            let rt = tokio::runtime::Handle::current();
            let store = rt.block_on(super::postgres_store::PostgresStore::new(&database_url))?;
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
