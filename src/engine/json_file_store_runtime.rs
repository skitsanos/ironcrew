//! Construction and owned blocking-worker execution for the JSON store.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use super::run_history::shared_json_run_lock;
use crate::utils::error::{IronCrewError, Result};

/// JSON file-based store rooted at an `.ironcrew/` directory. Every operation
/// crosses an owned blocking-worker boundary before touching the synchronous
/// core.
pub struct JsonFileStore {
    pub(super) inner: Arc<JsonFileStoreCore>,
}

pub(super) struct JsonFileStoreCore {
    pub(super) runs_dir: PathBuf,
    pub(super) conversations_dir: PathBuf,
    pub(super) dialogs_dir: PathBuf,
    pub(super) audit_events_dir: PathBuf,
    pub(super) idempotency_dir: PathBuf,
    pub(super) lease: super::store::RunLeaseConfig,
    pub(super) run_lock: Arc<Mutex<()>>,
}

impl JsonFileStore {
    pub async fn open(ironcrew_dir: PathBuf) -> Result<Self> {
        tokio::task::spawn_blocking(move || Self::new(ironcrew_dir))
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "JSON store blocking task failed during initialization: {error}"
                ))
            })?
    }

    pub fn new(ironcrew_dir: PathBuf) -> Result<Self> {
        Self::new_with_lease_config(ironcrew_dir, super::store::RunLeaseConfig::from_env()?)
    }

    pub fn new_with_lease_config(
        ironcrew_dir: PathBuf,
        lease: super::store::RunLeaseConfig,
    ) -> Result<Self> {
        let runs_dir = ironcrew_dir.join("runs");
        let conversations_dir = ironcrew_dir.join("conversations");
        let dialogs_dir = ironcrew_dir.join("dialogs");
        let audit_events_dir = ironcrew_dir.join("audit_events");
        let idempotency_dir = ironcrew_dir.join("idempotency");
        for dir in [
            &runs_dir,
            &conversations_dir,
            &dialogs_dir,
            &audit_events_dir,
            &idempotency_dir,
        ] {
            std::fs::create_dir_all(dir)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
            }
        }
        Ok(Self {
            inner: Arc::new(JsonFileStoreCore {
                run_lock: shared_json_run_lock(&runs_dir),
                runs_dir,
                conversations_dir,
                dialogs_dir,
                audit_events_dir,
                idempotency_dir,
                lease,
            }),
        })
    }

    pub(super) async fn run_blocking<T, F>(&self, operation: &'static str, work: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&JsonFileStoreCore) -> Result<T> + Send + 'static,
    {
        let core = Arc::clone(&self.inner);
        tokio::task::spawn_blocking(move || work(core.as_ref()))
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "JSON store blocking task failed during {operation}: {error}"
                ))
            })?
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use super::JsonFileStore;
    use crate::engine::run_history::RunIntent;
    use crate::engine::store::StateStore;

    #[tokio::test(flavor = "current_thread")]
    async fn locked_json_store_does_not_stall_tokio_worker() {
        let directory = tempfile::tempdir().unwrap();
        let store = Arc::new(JsonFileStore::new(directory.path().to_path_buf()).unwrap());
        let blocking_store = Arc::clone(&store);
        let (locked_tx, locked_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let blocker = tokio::task::spawn_blocking(move || {
            let guard = blocking_store.inner.run_lock.lock().unwrap();
            locked_tx.send(()).unwrap();
            release_rx.blocking_recv().unwrap();
            drop(guard);
        });
        locked_rx.await.unwrap();
        let task_store = Arc::clone(&store);
        let save = tokio::spawn(async move {
            task_store
                .save_run_intent(RunIntent {
                    suggested_id: Some("blocking-boundary".into()),
                    flow_name: "boundary".into(),
                    flow: "boundary".into(),
                    started_at: chrono::Utc::now().to_rfc3339(),
                    agent_count: 0,
                    task_count: 0,
                    tags: Vec::new(),
                })
                .await
        });

        tokio::time::timeout(
            Duration::from_secs(1),
            tokio::time::sleep(Duration::from_millis(10)),
        )
        .await
        .expect("Tokio timer must advance while JSON lock waits off-runtime");
        release_tx.send(()).unwrap();
        blocker.await.unwrap();
        assert_eq!(save.await.unwrap().unwrap(), "blocking-boundary");
    }
}
