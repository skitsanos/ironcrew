//! Blocking initialization for local persistence backends.

use std::path::PathBuf;

use super::sqlite_store::SqliteStore;
use crate::utils::error::{IronCrewError, Result};

pub(super) async fn open_sqlite(db_path: PathBuf) -> Result<SqliteStore> {
    tokio::task::spawn_blocking(move || {
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
            }
        }
        SqliteStore::new(db_path)
    })
    .await
    .map_err(|error| {
        IronCrewError::Validation(format!(
            "SQLite store blocking task failed during initialization: {error}"
        ))
    })?
}
