//! Strict MCP stdio transport with an assembled-line cap before JSON decode.

#[cfg(unix)]
use rmcp::{
    RoleClient,
    model::{ClientJsonRpcMessage, ServerJsonRpcMessage},
    service::{RxJsonRpcMessage, TxJsonRpcMessage},
    transport::Transport,
};
#[cfg(unix)]
use std::{io, process::Stdio, sync::Arc, time::Duration};
#[cfg(unix)]
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{ChildStdin, ChildStdout, Command},
    sync::Mutex,
};

pub(super) use crate::mcp::stdio_abort::StdioAbortHandle;
#[cfg(unix)]
use crate::mcp::{connection::PoisonWatch, protocol::inbound_is_allowed};

#[cfg(unix)]
const PROCESS_REAP_TIMEOUT: Duration = Duration::from_secs(1);

/// A child-process transport that never buffers more than one configured frame.
#[cfg(unix)]
pub(super) struct StrictStdioTransport {
    reader: BufReader<ChildStdout>,
    writer: Arc<Mutex<Option<ChildStdin>>>,
    line: Vec<u8>,
    max_inbound_bytes: usize,
    abort: StdioAbortHandle,
    poison: PoisonWatch,
}

#[cfg(unix)]
impl StrictStdioTransport {
    pub(super) fn spawn(
        command: &mut Command,
        max_inbound_bytes: usize,
        poison: PoisonWatch,
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
            supervisor_abort.abort();
            reaped_tx.send_replace(true);
        });
        Ok((
            Self {
                reader: BufReader::new(stdout),
                writer: Arc::new(Mutex::new(Some(stdin))),
                line: Vec::new(),
                max_inbound_bytes,
                abort: abort.clone(),
                poison,
            },
            abort,
        ))
    }

    async fn receive_bounded_line(&mut self) -> io::Result<Option<&[u8]>> {
        loop {
            let available = self.reader.fill_buf().await?;
            if available.is_empty() {
                if !self.line.is_empty() {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "MCP stdio closed with an incomplete JSON message",
                    ));
                }
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
        self.abort.wait_reaped().await
    }
}

#[cfg(unix)]
impl Transport<RoleClient> for StrictStdioTransport {
    type Error = io::Error;

    fn send(
        &mut self,
        item: TxJsonRpcMessage<RoleClient>,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
        let writer = Arc::clone(&self.writer);
        let poison = self.poison.clone();
        async move {
            if poison.is_poisoned() {
                return Err(io::Error::new(
                    io::ErrorKind::NotConnected,
                    "MCP connection was closed",
                ));
            }
            if let Err(error) = Self::validate_outbound(&item) {
                poison.poison();
                return Err(error);
            }
            let mut encoded = serde_json::to_vec(&item).map_err(|error| {
                poison.poison();
                io::Error::other(error)
            })?;
            encoded.push(b'\n');
            let mut writer = writer.lock().await;
            if poison.is_poisoned() {
                return Err(io::Error::new(
                    io::ErrorKind::NotConnected,
                    "MCP connection was closed",
                ));
            }
            let writer = writer
                .as_mut()
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "transport closed"))?;
            if let Err(error) = writer.write_all(&encoded).await {
                poison.poison();
                return Err(error);
            }
            if let Err(error) = writer.flush().await {
                poison.poison();
                return Err(error);
            }
            Ok(())
        }
    }

    async fn receive(&mut self) -> Option<RxJsonRpcMessage<RoleClient>> {
        let line = match self.receive_bounded_line().await {
            Ok(Some(line)) => line,
            Ok(None) => return None,
            Err(error) => {
                tracing::warn!(%error, "closing strict MCP stdio transport");
                self.poison.poison();
                self.abort.abort();
                return None;
            }
        };
        let parsed = serde_json::from_slice::<ServerJsonRpcMessage>(line);
        self.line.clear();
        match parsed {
            Ok(message) if !inbound_is_allowed(&message) => {
                tracing::warn!("inbound message violates strict MCP 2026 surface");
                self.poison.poison();
                self.abort.abort();
                None
            }
            Ok(message) => Some(message),
            Err(error) => {
                tracing::warn!(%error, "closing on invalid bounded MCP stdio message");
                self.poison.poison();
                self.abort.abort();
                None
            }
        }
    }

    async fn close(&mut self) -> Result<(), Self::Error> {
        self.reap().await
    }
}

#[cfg(unix)]
impl Drop for StrictStdioTransport {
    fn drop(&mut self) {
        self.abort.abort();
    }
}
