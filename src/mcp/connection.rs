//! One-shot connection poison shared by transport and in-flight operations.

use std::sync::{Arc, Mutex};

use rmcp::service::RunningServiceCancellationToken;
use tokio::sync::watch;

use crate::mcp::stdio_transport::StdioAbortHandle;
use crate::utils::error::{IronCrewError, Result};

#[derive(Clone)]
pub(super) struct PoisonSignal {
    sender: Arc<watch::Sender<bool>>,
}

#[derive(Clone)]
pub(super) struct PoisonWatch {
    sender: Arc<watch::Sender<bool>>,
    receiver: watch::Receiver<bool>,
}

impl PoisonSignal {
    pub(super) fn channel() -> (Self, PoisonWatch) {
        let (sender, receiver) = watch::channel(false);
        let sender = Arc::new(sender);
        (
            Self {
                sender: Arc::clone(&sender),
            },
            PoisonWatch {
                sender: Arc::clone(&sender),
                receiver,
            },
        )
    }

    pub(super) fn poison(&self) {
        self.sender.send_replace(true);
    }

    fn is_poisoned(&self) -> bool {
        *self.sender.borrow()
    }
}

impl PoisonWatch {
    pub(super) fn is_poisoned(&self) -> bool {
        *self.receiver.borrow()
    }

    pub(super) async fn poisoned(&mut self) {
        while !self.is_poisoned() && self.receiver.changed().await.is_ok() {}
    }

    pub(super) fn poison(&self) {
        self.sender.send_replace(true);
    }
}

#[derive(Clone)]
pub(super) struct ConnectionPoison {
    signal: PoisonSignal,
    service: Arc<Mutex<Option<RunningServiceCancellationToken>>>,
    stdio: Option<StdioAbortHandle>,
}

impl ConnectionPoison {
    pub(super) fn new(
        signal: PoisonSignal,
        service: RunningServiceCancellationToken,
        stdio: Option<StdioAbortHandle>,
    ) -> Self {
        Self {
            signal,
            service: Arc::new(Mutex::new(Some(service))),
            stdio,
        }
    }

    pub(super) fn poison(&self) {
        self.signal.poison();
        if let Some(stdio) = &self.stdio {
            stdio.abort();
        }
        if let Some(service) = self.service.lock().expect("MCP poison lock").take() {
            service.cancel();
        }
    }

    pub(super) fn is_poisoned(&self) -> bool {
        self.signal.is_poisoned()
    }

    async fn wait_local_closed(&self) {
        if let Some(stdio) = &self.stdio {
            let _ = stdio.wait_reaped().await;
        }
    }
}

pub(super) struct InFlightGuard {
    poison: ConnectionPoison,
    armed: bool,
}

pub(super) struct DiscoveryGuard {
    signal: PoisonSignal,
    stdio: Option<StdioAbortHandle>,
    armed: bool,
}

impl DiscoveryGuard {
    pub(super) fn new(signal: PoisonSignal, stdio: Option<StdioAbortHandle>) -> Self {
        Self {
            signal,
            stdio,
            armed: true,
        }
    }

    pub(super) fn disarm(mut self) {
        self.armed = false;
    }

    pub(super) async fn abort(&self) {
        self.signal.poison();
        if let Some(stdio) = &self.stdio {
            stdio.abort();
            let _ = stdio.wait_reaped().await;
        }
    }

    pub(super) async fn run<T, E>(
        self,
        timeout: std::time::Duration,
        server: &str,
        transport: &str,
        future: impl Future<Output = std::result::Result<T, E>>,
    ) -> Result<T>
    where
        E: std::fmt::Display,
    {
        let result = match tokio::time::timeout(timeout, future).await {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(error)) => Err(IronCrewError::Mcp {
                server: server.to_owned(),
                message: format!("MCP {transport} discovery failed: {error}"),
            }),
            Err(_) => Err(IronCrewError::Mcp {
                server: server.to_owned(),
                message: format!(
                    "MCP {transport} discovery timed out after {} seconds",
                    timeout.as_secs()
                ),
            }),
        };
        if result.is_err() {
            self.abort().await;
        } else {
            self.disarm();
        }
        result
    }
}

impl Drop for DiscoveryGuard {
    fn drop(&mut self) {
        if self.armed {
            self.signal.poison();
            if let Some(stdio) = &self.stdio {
                stdio.abort();
            }
        }
    }
}

impl InFlightGuard {
    pub(super) fn new(poison: ConnectionPoison) -> Self {
        Self {
            poison,
            armed: true,
        }
    }

    pub(super) fn disarm(mut self) {
        self.armed = false;
    }

    pub(super) fn poison(&self) {
        self.poison.poison();
    }

    pub(super) async fn poison_and_wait(&self, deadline: tokio::time::Instant) {
        self.poison.poison();
        let _ = tokio::time::timeout_at(deadline, self.poison.wait_local_closed()).await;
    }
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        if self.armed {
            self.poison.poison();
        }
    }
}
