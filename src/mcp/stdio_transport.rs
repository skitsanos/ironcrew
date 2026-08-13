//! Strict, pre-materialization-bounded MCP stdio transport.

use std::{
    io,
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicI32, Ordering},
    },
    time::Duration,
};

use rmcp::{
    RoleClient,
    model::{ClientJsonRpcMessage, ServerJsonRpcMessage},
    service::{RxJsonRpcMessage, TxJsonRpcMessage},
    transport::Transport,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{ChildStdin, ChildStdout, Command},
    sync::{Mutex, watch},
};

use crate::mcp::protocol::inbound_is_allowed;

const PROCESS_REAP_TIMEOUT: Duration = Duration::from_secs(1);

/// Synchronous process-group poison used by timeout and cancelled caller paths.
#[derive(Clone, Debug)]
pub(super) struct StdioAbortHandle {
    pgid: Arc<AtomicI32>,
    abort: watch::Sender<bool>,
    reaped: watch::Receiver<bool>,
}

impl StdioAbortHandle {
    fn new(process_id: Option<u32>) -> (Self, watch::Receiver<bool>, watch::Sender<bool>) {
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

    pub(super) async fn wait_reaped(&self) {
        let mut reaped = self.reaped.clone();
        while !*reaped.borrow() && reaped.changed().await.is_ok() {}
    }
}

/// A child-process transport that never buffers more than one configured frame.
pub(super) struct StrictStdioTransport {
    reader: BufReader<ChildStdout>,
    writer: Arc<Mutex<Option<ChildStdin>>>,
    line: Vec<u8>,
    max_inbound_bytes: usize,
    abort: StdioAbortHandle,
}

impl StrictStdioTransport {
    pub(super) fn spawn(
        command: &mut Command,
        max_inbound_bytes: usize,
    ) -> io::Result<(Self, StdioAbortHandle)> {
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        #[cfg(unix)]
        command.process_group(0);

        let mut child = command.spawn()?;
        let (abort, mut abort_rx, reaped_tx) = StdioAbortHandle::new(child.id());
        let supervisor_abort = abort.clone();
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("MCP child stdout was not piped"))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| io::Error::other("MCP child stdin was not piped"))?;
        tokio::spawn(async move {
            tokio::select! {
                result = child.wait() => {
                    if let Err(error) = result {
                        tracing::debug!(%error, "MCP child wait failed");
                    }
                }
                _ = abort_rx.changed() => {
                    let _ = child.start_kill();
                    if tokio::time::timeout(PROCESS_REAP_TIMEOUT, child.wait()).await.is_err() {
                        let _ = child.kill().await;
                        let _ = child.wait().await;
                    }
                }
            }
            supervisor_abort.pgid.store(0, Ordering::Release);
            reaped_tx.send_replace(true);
        });
        Ok((
            Self {
                reader: BufReader::new(stdout),
                writer: Arc::new(Mutex::new(Some(stdin))),
                line: Vec::new(),
                max_inbound_bytes,
                abort: abort.clone(),
            },
            abort,
        ))
    }

    async fn receive_bounded_line(&mut self) -> io::Result<Option<&[u8]>> {
        loop {
            let available = self.reader.fill_buf().await?;
            if available.is_empty() {
                return Ok(None);
            }
            let newline = available.iter().position(|byte| *byte == b'\n');
            let take = newline.unwrap_or(available.len());
            if self.line.len().saturating_add(take) > self.max_inbound_bytes {
                return Err(io::Error::new(
                    io::ErrorKind::FileTooLarge,
                    format!("MCP stdio message exceeds {} bytes", self.max_inbound_bytes),
                ));
            }
            self.line.extend_from_slice(&available[..take]);
            self.reader.consume(take + usize::from(newline.is_some()));
            if newline.is_some() {
                if self.line.last() == Some(&b'\r') {
                    self.line.pop();
                }
                if self.line.is_empty() {
                    continue;
                }
                return Ok(Some(&self.line));
            }
        }
    }

    fn validate_outbound(message: &ClientJsonRpcMessage) -> io::Result<()> {
        let allowed = match message {
            ClientJsonRpcMessage::Request(request) => matches!(
                request.request.method(),
                "server/discover" | "tools/list" | "tools/call"
            ),
            ClientJsonRpcMessage::Notification(notification) => matches!(
                notification.notification,
                rmcp::model::ClientNotification::CancelledNotification(_)
            ),
            ClientJsonRpcMessage::Response(_) | ClientJsonRpcMessage::Error(_) => false,
        };
        if allowed {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "outbound message is outside IronCrew's strict MCP 2026 surface",
            ))
        }
    }

    async fn reap(&mut self) -> io::Result<()> {
        self.abort.abort();
        self.abort.wait_reaped().await;
        Ok(())
    }
}

impl Transport<RoleClient> for StrictStdioTransport {
    type Error = io::Error;

    fn send(
        &mut self,
        item: TxJsonRpcMessage<RoleClient>,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
        let writer = Arc::clone(&self.writer);
        async move {
            Self::validate_outbound(&item)?;
            let mut encoded = serde_json::to_vec(&item).map_err(io::Error::other)?;
            encoded.push(b'\n');
            let mut writer = writer.lock().await;
            let writer = writer
                .as_mut()
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "transport closed"))?;
            writer.write_all(&encoded).await?;
            writer.flush().await
        }
    }

    async fn receive(&mut self) -> Option<RxJsonRpcMessage<RoleClient>> {
        let line = match self.receive_bounded_line().await {
            Ok(Some(line)) => line,
            Ok(None) => return None,
            Err(error) => {
                tracing::warn!(%error, "closing strict MCP stdio transport");
                self.abort.abort();
                return None;
            }
        };
        let parsed = serde_json::from_slice::<ServerJsonRpcMessage>(line);
        self.line.clear();
        match parsed {
            Ok(message) if !inbound_is_allowed(&message) => {
                tracing::warn!("inbound message violates strict MCP 2026 surface");
                self.abort.abort();
                None
            }
            Ok(message) => Some(message),
            Err(error) => {
                tracing::warn!(%error, "closing on invalid bounded MCP stdio message");
                self.abort.abort();
                None
            }
        }
    }

    async fn close(&mut self) -> Result<(), Self::Error> {
        self.reap().await
    }
}

impl Drop for StrictStdioTransport {
    fn drop(&mut self) {
        self.abort.abort();
    }
}
