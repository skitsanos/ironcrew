use async_trait::async_trait;

use crate::engine::run_history::{ListRunsFilter, RunRecord, RunSummary};
use crate::engine::sessions::{ConversationRecord, ConversationSummary, DialogStateRecord};
use crate::utils::error::Result;

/// Pluggable storage backend for run records and persistent sessions
/// (conversations and dialogs).
#[async_trait]
pub trait StateStore: Send + Sync {
    // ─── Run history ────────────────────────────────────────────────────────

    async fn save_run(&self, record: &RunRecord) -> Result<String>;
    async fn get_run(&self, run_id: &str) -> Result<RunRecord>;

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

    // ─── Persistent sessions ────────────────────────────────────────────────

    /// Upsert a conversation record. The `(flow_path, id)` pair is the
    /// effective unique key — a legacy record with `flow_path = None` is
    /// only reachable when the caller also passes `None` (by convention, a
    /// global-scope lookup, as used by the `ironcrew inspect` CLI).
    async fn save_conversation(&self, record: &ConversationRecord) -> Result<()>;
    /// Look up a conversation by `(flow_path, id)`. Returns `Ok(None)` when
    /// no record matches — which is how `crew:conversation({id = ...})`
    /// distinguishes a fresh session from a resumed one. When `flow_path`
    /// is `Some(..)`, the query is strictly scoped: a record belonging to a
    /// different flow (or to no flow, `flow_path = NULL`) is invisible.
    /// When `flow_path` is `None`, any matching `id` is returned — only
    /// use this form for admin paths that are not tied to a specific flow.
    async fn get_conversation(
        &self,
        flow_path: Option<&str>,
        id: &str,
    ) -> Result<Option<ConversationRecord>>;
    /// Delete a conversation scoped by `(flow_path, id)`. Same semantics as
    /// `get_conversation`: a delete with `flow_path = Some(..)` will not
    /// touch records belonging to another flow.
    async fn delete_conversation(&self, flow_path: Option<&str>, id: &str) -> Result<()>;

    /// Paginated list of conversation summaries, newest first by updated_at.
    /// When `flow_path` is `Some`, only records whose `flow_path` matches are
    /// returned. When `None`, all records are returned regardless of flow.
    async fn list_conversations(
        &self,
        flow_path: Option<&str>,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<ConversationSummary>>;

    /// Count of conversations matching the flow filter.
    async fn count_conversations(&self, flow_path: Option<&str>) -> Result<u64>;

    /// Upsert a dialog state record. Keyed by `(flow_path, id)` — same
    /// scoping rules as conversations (see `get_conversation` docs).
    async fn save_dialog_state(&self, record: &DialogStateRecord) -> Result<()>;
    /// Look up a dialog by `(flow_path, id)`. Returns `Ok(None)` when no
    /// record matches. Flow-scope semantics match `get_conversation`:
    /// `Some(path)` requires an exact flow match (legacy records with
    /// `flow_path = None` are invisible); `None` is the global admin
    /// lookup.
    async fn get_dialog_state(
        &self,
        flow_path: Option<&str>,
        id: &str,
    ) -> Result<Option<DialogStateRecord>>;
    /// Delete a dialog by `(flow_path, id)`. Never touches another
    /// flow's record when `flow_path` is `Some(..)`.
    async fn delete_dialog_state(&self, flow_path: Option<&str>, id: &str) -> Result<()>;
}

/// Create a StateStore based on environment configuration.
///
/// `IRONCREW_STORE=json` (default) — JSON files in the given directory
/// `IRONCREW_STORE=sqlite` — SQLite database
/// `IRONCREW_STORE=postgres` — PostgreSQL 15+ (requires `postgres` feature)
/// `IRONCREW_STORE_PATH=<path>` — path for SQLite db (default: `<default_dir>/ironcrew.db`)
/// `DATABASE_URL=postgres://...` — PostgreSQL connection string
/// `IRONCREW_PG_TABLE_PREFIX=prefix_` — table prefix for shared databases
///
/// Returns an `Arc` so the same instance can be shared across the crew's
/// `run()`, `conversation()`, and `dialog()` call paths without re-opening
/// the underlying connection/pool.
pub async fn create_store(
    default_dir: std::path::PathBuf,
) -> Result<std::sync::Arc<dyn StateStore>> {
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
            Ok(std::sync::Arc::new(super::sqlite_store::SqliteStore::new(
                db_path,
            )?))
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
            Ok(std::sync::Arc::new(store))
        }
        #[cfg(not(feature = "postgres"))]
        "postgres" | "postgresql" => Err(crate::utils::error::IronCrewError::Validation(
            "PostgreSQL backend requires building with --features postgres".into(),
        )),
        _ => {
            // JsonFileStore creates `runs/`, `conversations/`, and `dialogs/`
            // subdirectories inside the given `.ironcrew/` root.
            Ok(std::sync::Arc::new(super::run_history::JsonFileStore::new(
                default_dir,
            )?))
        }
    }
}
