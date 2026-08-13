//! Transport-agnostic MCP client wrapper.
//!
//! Wraps either a stdio or HTTP rmcp `RunningService` behind a uniform API
//! so the rest of IronCrew never needs to deal with transport generics.

use futures::future::BoxFuture;
use rmcp::{Peer, RoleClient, service::RunningService};
use std::time::Duration;
use tokio::sync::Mutex;

use crate::mcp::connection::{ConnectionPoison, PoisonSignal};
use crate::mcp::execution_policy::McpCallPolicy;
use crate::mcp::http_tool_headers::HttpToolHeaderRegistry;
use crate::utils::error::{IronCrewError, Result};

const DEFAULT_SHUTDOWN_TIMEOUT_SECS: u64 = 5;
const MAX_TIMEOUT_SECS: u64 = 3_600;

pub(super) fn mcp_error(message: impl Into<String>) -> IronCrewError {
    IronCrewError::Mcp {
        server: String::new(),
        message: message.into(),
    }
}

fn bounded_env_u64(name: &str, default: u64, max: u64) -> Result<u64> {
    let value = match std::env::var(name) {
        Ok(raw) => raw
            .parse::<u64>()
            .map_err(|_| mcp_error(format!("{name} must be an integer from 1 to {max}")))?,
        Err(_) => default,
    };
    if !(1..=max).contains(&value) {
        return Err(mcp_error(format!("{name} must be from 1 to {max}")));
    }
    Ok(value)
}

pub(super) fn configured_timeout(name: &str, default: u64) -> Result<Duration> {
    Ok(Duration::from_secs(bounded_env_u64(
        name,
        default,
        MAX_TIMEOUT_SECS,
    )?))
}

// ── shutdown handle ───────────────────────────────────────────────────────────

/// Type-erased async shutdown closure that owns the `RunningService`.
///
/// Awaiting this signals the service's cancellation token, drives the
/// service loop to completion, and drops the transport (killing its owned
/// stdio process group and reaping the direct child).
type ShutdownFn = Box<dyn FnOnce() -> BoxFuture<'static, ()> + Send + Sync>;

// ── McpClient ────────────────────────────────────────────────────────────────

/// Type-erased MCP client.
///
/// Holds the `Peer<RoleClient>` (cheap-clone RPC handle) and an async
/// shutdown closure that owns the underlying `RunningService` so it can
/// be torn down deterministically.
pub struct McpClient {
    pub(super) peer: Peer<RoleClient>,
    shutdown: Mutex<Option<ShutdownFn>>,
    pub(super) call_policy: McpCallPolicy,
    pub(super) poison: ConnectionPoison,
    pub(super) operation: Mutex<()>,
    pub(super) tool_headers: Option<HttpToolHeaderRegistry>,
}

impl McpClient {
    pub(super) fn from_service<S>(
        service: RunningService<RoleClient, S>,
        poison_signal: PoisonSignal,
        stdio_abort: Option<crate::mcp::stdio_transport::StdioAbortHandle>,
        call_policy: McpCallPolicy,
        tool_headers: Option<HttpToolHeaderRegistry>,
    ) -> Self
    where
        S: rmcp::Service<RoleClient> + 'static,
    {
        let peer = service.peer().clone();
        let poison = ConnectionPoison::new(
            poison_signal.clone(),
            service.cancellation_token(),
            stdio_abort,
        );
        // Own the service inside the shutdown closure. When awaited, it
        // consumes the service via `cancel()` which signals the token,
        // awaits the service loop's exit, and drops the transport.
        let shutdown: ShutdownFn = Box::new(move || {
            Box::pin(async move {
                poison_signal.poison();
                if let Err(e) = service.cancel().await {
                    tracing::debug!(error = %e, "MCP service cancel returned error");
                }
            })
        });
        McpClient {
            peer,
            shutdown: Mutex::new(Some(shutdown)),
            call_policy,
            poison,
            operation: Mutex::new(()),
            tool_headers,
        }
    }

    pub(super) fn call_policy(&self) -> McpCallPolicy {
        self.call_policy
    }

    pub(super) fn transport_execution_definition(&self, name: &str) -> Result<serde_json::Value> {
        match &self.tool_headers {
            Some(registry) => registry
                .plan_definition(name)
                .map(|plan| serde_json::json!({"mode": "http-2026", "headers": plan}))
                .ok_or_else(|| {
                    mcp_error(format!(
                        "MCP tool `{name}` has no committed HTTP header plan"
                    ))
                }),
            None => Ok(serde_json::json!({"mode": "stdio-2026", "headers": null})),
        }
    }

    /// Graceful async shutdown — awaits the service loop's exit and drops the
    /// transport (killing its owned stdio process group and reaping the direct
    /// child). Idempotent: a second call is a no-op. Called by
    /// `McpConnectionManager::shutdown`.
    #[allow(dead_code)]
    pub async fn shutdown(&self) {
        let f = self.shutdown.lock().await.take();
        if let Some(f) = f {
            let timeout = configured_timeout(
                "IRONCREW_MCP_SHUTDOWN_TIMEOUT_SECS",
                DEFAULT_SHUTDOWN_TIMEOUT_SECS,
            )
            .unwrap_or_else(|error| {
                tracing::warn!(error = %error, "Invalid MCP shutdown timeout; using default");
                Duration::from_secs(DEFAULT_SHUTDOWN_TIMEOUT_SECS)
            });
            if tokio::time::timeout(timeout, f()).await.is_err() {
                tracing::warn!(
                    timeout_seconds = timeout.as_secs(),
                    "MCP shutdown timed out; dropping transport"
                );
            }
        }
    }

    /// Best-effort synchronous shutdown for `Drop` paths. Spawns the async
    /// shutdown on the current Tokio runtime. If no runtime is active
    /// (e.g. the runtime is already winding down), the service is dropped
    /// on the current thread, which still tears down the transport — just
    /// without waiting for the loop to finish.
    pub fn shutdown_blocking(&self) {
        // Try to take the shutdown fn synchronously. `try_lock` is fine
        // because we are the sole holder in normal shutdown flow.
        let f = match self.shutdown.try_lock() {
            Ok(mut guard) => guard.take(),
            Err(_) => return,
        };
        let Some(f) = f else { return };
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn(async move {
                    let timeout = configured_timeout(
                        "IRONCREW_MCP_SHUTDOWN_TIMEOUT_SECS",
                        DEFAULT_SHUTDOWN_TIMEOUT_SECS,
                    )
                    .unwrap_or(Duration::from_secs(DEFAULT_SHUTDOWN_TIMEOUT_SECS));
                    let _ = tokio::time::timeout(timeout, f()).await;
                });
            }
            Err(_) => {
                // No runtime — drop the owned service on this thread.
                drop(f);
            }
        }
    }
}
