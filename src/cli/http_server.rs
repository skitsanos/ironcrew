use std::convert::Infallible;
use std::io;
use std::net::SocketAddr;
use std::pin::{Pin, pin};
use std::sync::Arc;
use std::task::{Context, Poll};

use axum::Router;
use axum::body::Body;
use hyper::body::Incoming;
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use hyper_util::server::conn::auto::Builder;
use hyper_util::service::TowerToHyperService;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Semaphore, watch};
use tokio::task::JoinSet;
use tower::{Service, ServiceExt};

use super::http_limits::HttpLimits;

const HTTP2_PREFACE: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";

pub(super) async fn serve(
    listener: TcpListener,
    app: Router,
    mut shutdown: watch::Receiver<bool>,
    limits: HttpLimits,
) -> io::Result<()> {
    let mut make_service = app.into_make_service_with_connect_info::<SocketAddr>();
    let permits = Arc::new(Semaphore::new(limits.max_connections));
    let (connection_shutdown, _) = watch::channel(false);
    let mut connections = JoinSet::new();

    loop {
        reap_finished(&mut connections);
        let permit = tokio::select! {
            permit = permits.clone().acquire_owned() => {
                permit.expect("HTTP connection semaphore cannot close")
            }
            _ = shutdown.wait_for(|stopping| *stopping) => break,
        };
        let accepted = tokio::select! {
            accepted = listener.accept() => accepted,
            _ = shutdown.wait_for(|stopping| *stopping) => {
                drop(permit);
                break;
            }
        };
        let (stream, peer) = match accepted {
            Ok(connection) => connection,
            Err(error) => {
                drop(permit);
                if !is_connection_error(&error) {
                    tracing::error!(%error, "HTTP accept failed; retrying");
                    tokio::select! {
                        _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {}
                        _ = shutdown.wait_for(|stopping| *stopping) => break,
                    }
                }
                continue;
            }
        };
        if let Err(error) = stream.set_nodelay(true) {
            tracing::trace!(%error, %peer, "Failed to set TCP_NODELAY");
        }

        let service = Service::<SocketAddr>::call(&mut make_service, peer)
            .await
            .unwrap_or_else(|error: Infallible| match error {})
            .map_request(|request: hyper::Request<Incoming>| request.map(Body::new));
        let mut connection_stop = connection_shutdown.subscribe();
        connections.spawn(async move {
            let _permit = permit;
            let io = TokioIo::new(PrefaceTimeoutIo::new(stream, limits.header_read_timeout));
            let mut builder = Builder::new(TokioExecutor::new());
            builder
                .http1()
                .timer(TokioTimer::new())
                .header_read_timeout(limits.header_read_timeout);
            builder
                .http2()
                .enable_connect_protocol()
                .keep_alive_interval(limits.header_read_timeout)
                .keep_alive_timeout(limits.header_read_timeout);
            let service = TowerToHyperService::new(service);
            let mut connection = pin!(builder.serve_connection_with_upgrades(io, service));
            tokio::select! {
                result = &mut connection => log_connection_result(peer, result),
                _ = async {
                    connection_stop
                        .wait_for(|stopping| *stopping)
                        .await
                        .map(drop)
                } => {
                    connection.as_mut().graceful_shutdown();
                    log_connection_result(peer, connection.await);
                }
            }
        });
    }

    drop(listener);
    connection_shutdown.send_replace(true);
    while let Some(result) = connections.join_next().await {
        if let Err(error) = result {
            tracing::warn!(%error, "HTTP connection task failed");
        }
    }
    Ok(())
}

fn reap_finished(connections: &mut JoinSet<()>) {
    while let Some(result) = connections.try_join_next() {
        if let Err(error) = result {
            tracing::warn!(%error, "HTTP connection task failed");
        }
    }
}

fn log_connection_result<E>(peer: SocketAddr, result: Result<(), E>)
where
    E: std::fmt::Display,
{
    if let Err(error) = result {
        tracing::trace!(%error, %peer, "HTTP connection closed with a protocol error");
    }
}

fn is_connection_error(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::ConnectionRefused
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::ConnectionReset
    )
}

struct PrefaceTimeoutIo {
    stream: TcpStream,
    deadline: Option<Pin<Box<tokio::time::Sleep>>>,
    matched: usize,
}

impl PrefaceTimeoutIo {
    fn new(stream: TcpStream, timeout: std::time::Duration) -> Self {
        Self {
            stream,
            deadline: Some(Box::pin(tokio::time::sleep(timeout))),
            matched: 0,
        }
    }

    fn inspect(&mut self, bytes: &[u8]) {
        for byte in bytes {
            if *byte != HTTP2_PREFACE[self.matched] {
                self.deadline = None;
                return;
            }
            self.matched += 1;
            if self.matched == HTTP2_PREFACE.len() {
                self.deadline = None;
                return;
            }
        }
    }
}

impl AsyncRead for PrefaceTimeoutIo {
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if self
            .deadline
            .as_mut()
            .is_some_and(|deadline| deadline.as_mut().poll(context).is_ready())
        {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "HTTP protocol preface read timed out",
            )));
        }
        let filled_before = buffer.filled().len();
        let result = Pin::new(&mut self.stream).poll_read(context, buffer);
        if matches!(result, Poll::Ready(Ok(()))) && self.deadline.is_some() {
            self.inspect(&buffer.filled()[filled_before..]);
        }
        result
    }
}

impl AsyncWrite for PrefaceTimeoutIo {
    fn poll_write(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.stream).poll_write(context, buffer)
    }

    fn poll_flush(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_flush(context)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_shutdown(context)
    }

    fn is_write_vectored(&self) -> bool {
        self.stream.is_write_vectored()
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffers: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.stream).poll_write_vectored(context, buffers)
    }
}

#[cfg(test)]
mod tests;
