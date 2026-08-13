//! Cross-platform handle for fail-closed MCP stdio process cleanup.

use std::{
    io,
    sync::{
        Arc,
        atomic::{AtomicI32, Ordering},
    },
};

use tokio::sync::watch;

/// Synchronous process-group poison used by timeout and cancelled caller paths.
#[derive(Clone, Debug)]
pub(super) struct StdioAbortHandle {
    pgid: Arc<AtomicI32>,
    abort: watch::Sender<bool>,
    reaped: watch::Receiver<bool>,
}

impl StdioAbortHandle {
    #[cfg(unix)]
    pub(super) fn new(
        process_id: Option<u32>,
    ) -> (Self, watch::Receiver<bool>, watch::Sender<bool>) {
        let pgid = process_id
            .and_then(|id| i32::try_from(id).ok())
            .unwrap_or_default();
        let (abort, abort_rx) = watch::channel(false);
        let (reaped_tx, reaped) = watch::channel(false);
        (
            Self {
                pgid: Arc::new(AtomicI32::new(pgid)),
                abort,
                reaped,
            },
            abort_rx,
            reaped_tx,
        )
    }

    pub(super) fn abort(&self) {
        let pgid = self.pgid.swap(0, Ordering::AcqRel);
        #[cfg(unix)]
        if pgid > 0 {
            let _ = nix::sys::signal::killpg(
                nix::unistd::Pid::from_raw(pgid),
                nix::sys::signal::Signal::SIGKILL,
            );
        }
        #[cfg(not(unix))]
        let _ = pgid;
        self.abort.send_replace(true);
    }

    pub(super) async fn wait_reaped(&self) -> io::Result<()> {
        let mut reaped = self.reaped.clone();
        loop {
            if *reaped.borrow() {
                return Ok(());
            }
            reaped.changed().await.map_err(|_| {
                io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "MCP reaper exited before confirming process-group cleanup",
                )
            })?;
        }
    }
}
