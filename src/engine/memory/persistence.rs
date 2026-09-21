//! Owned blocking-worker helpers for persistent crew memory.

use std::io::Write;
use std::path::{Path, PathBuf};

use super::{MemoryConfig, MemoryStore};
use crate::utils::error::{IronCrewError, Result};

impl MemoryStore {
    /// Open and parse a persistent memory snapshot off the Tokio worker pool.
    pub async fn persistent_with_config_async(path: PathBuf, config: MemoryConfig) -> Result<Self> {
        tokio::task::spawn_blocking(move || Self::persistent_with_config(path, config))
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "memory snapshot blocking task failed while loading: {error}"
                ))
            })?
    }

    pub(super) fn atomic_save(path: &Path, json: &[u8]) -> Result<()> {
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        std::fs::create_dir_all(parent).map_err(IronCrewError::Io)?;
        #[cfg(unix)]
        if parent != Path::new(".") {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))
                .map_err(IronCrewError::Io)?;
        }
        let file_name = path.file_name().ok_or_else(|| {
            IronCrewError::Validation(format!("invalid memory path '{}'", path.display()))
        })?;
        let temporary = parent.join(format!(
            ".{}.{}.tmp",
            file_name.to_string_lossy(),
            uuid::Uuid::new_v4()
        ));
        let write_result = (|| -> std::io::Result<()> {
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options.open(&temporary)?;
            file.write_all(json)?;
            file.sync_all()?;
            std::fs::rename(&temporary, path)?;
            std::fs::File::open(parent)?.sync_all()?;
            Ok(())
        })();
        if write_result.is_err() {
            let _ = std::fs::remove_file(&temporary);
        }
        write_result.map_err(IronCrewError::Io)
    }
}
