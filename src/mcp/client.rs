//! Transport-agnostic MCP client wrapper.
//!
//! Wraps either a stdio or HTTP rmcp `RunningService` behind a uniform API
//! so the rest of IronCrew never needs to deal with transport generics.

use axum::http::{HeaderName, HeaderValue};
use futures::future::BoxFuture;
use rmcp::{
    Peer, RoleClient, ServiceExt,
    model::{CallToolRequestParams, ClientCapabilities, ClientInfo, Implementation},
    service::RunningService,
    transport::streamable_http_client::StreamableHttpClientTransportConfig,
    transport::{ConfigureCommandExt, StreamableHttpClientTransport, TokioChildProcess},
};
use std::collections::HashMap;
use tokio::process::Command;
use tokio::sync::Mutex;

use crate::mcp::config::{McpServerConfig, McpTransportConfig};
use crate::utils::error::{IronCrewError, Result};

// ── safe-env helpers ──────────────────────────────────────────────────────────

/// Env vars that are safe to forward to MCP child processes by default.
const SAFE_ENV_KEYS: &[&str] = &["PATH", "HOME", "USER", "LANG"];

fn build_child_env(config_env: &HashMap<String, String>, inherit: bool) -> HashMap<String, String> {
    let mut env: HashMap<String, String> = if inherit {
        std::env::vars().collect()
    } else {
        // Allow only whitelisted keys from the parent environment
        SAFE_ENV_KEYS
            .iter()
            .filter_map(|k| std::env::var(k).ok().map(|v| (k.to_string(), v)))
            .chain(
                // Also forward LC_* variables
                std::env::vars().filter(|(k, _)| k.starts_with("LC_")),
            )
            .collect()
    };
    // Layer user-supplied overrides on top
    env.extend(config_env.clone());
    env
}

// ── shutdown handle ───────────────────────────────────────────────────────────

/// Type-erased async shutdown closure that owns the `RunningService`.
///
/// Awaiting this signals the service's cancellation token, drives the
/// service loop to completion, and drops the transport (reaping stdio
/// children via pipe closure).
type ShutdownFn = Box<dyn FnOnce() -> BoxFuture<'static, ()> + Send + Sync>;

// ── McpClient ────────────────────────────────────────────────────────────────

/// Type-erased MCP client.
///
/// Holds the `Peer<RoleClient>` (cheap-clone RPC handle) and an async
/// shutdown closure that owns the underlying `RunningService` so it can
/// be torn down deterministically.
pub struct McpClient {
    peer: Peer<RoleClient>,
    shutdown: Mutex<Option<ShutdownFn>>,
}

impl McpClient {
    fn from_service<S>(service: RunningService<RoleClient, S>) -> Self
    where
        S: rmcp::Service<RoleClient> + 'static,
    {
        let peer = service.peer().clone();
        // Own the service inside the shutdown closure. When awaited, it
        // consumes the service via `cancel()` which signals the token,
        // awaits the service loop's exit, and drops the transport.
        let shutdown: ShutdownFn = Box::new(move || {
            Box::pin(async move {
                if let Err(e) = service.cancel().await {
                    tracing::debug!(error = %e, "MCP service cancel returned error");
                }
            })
        });
        McpClient {
            peer,
            shutdown: Mutex::new(Some(shutdown)),
        }
    }

    /// Connect using a `McpServerConfig`, respecting all security constraints.
    pub async fn connect(cfg: &McpServerConfig) -> Result<Self> {
        match &cfg.transport {
            McpTransportConfig::Stdio { command, args, env } => {
                let child_env = build_child_env(env, cfg.inherit_env);

                let transport = TokioChildProcess::new({
                    let mut cmd = Command::new(command);
                    cmd.args(args);
                    // Replace the environment entirely with the curated set
                    cmd.env_clear();
                    for (k, v) in &child_env {
                        cmd.env(k, v);
                    }
                    cmd.configure(|_| {})
                })
                .map_err(|e| IronCrewError::Mcp {
                    server: cfg.label.clone(),
                    message: format!("Failed to create stdio transport: {}", e),
                })?;

                let service: RunningService<RoleClient, ()> =
                    ().serve(transport).await.map_err(|e| IronCrewError::Mcp {
                        server: cfg.label.clone(),
                        message: format!("Handshake failed: {}", e),
                    })?;

                Ok(Self::from_service(service))
            }
            McpTransportConfig::Http { url, headers } => {
                let config = if headers.is_empty() {
                    StreamableHttpClientTransportConfig::with_uri(url.as_str())
                } else {
                    let mut header_map: HashMap<HeaderName, HeaderValue> = HashMap::new();
                    for (k, v) in headers {
                        let name = HeaderName::from_bytes(k.as_bytes()).map_err(|e| {
                            IronCrewError::Mcp {
                                server: cfg.label.clone(),
                                message: format!("Invalid header name '{}': {}", redact_key(k), e),
                            }
                        })?;
                        let value = HeaderValue::from_str(v).map_err(|e| IronCrewError::Mcp {
                            server: cfg.label.clone(),
                            message: format!("Invalid header value for '{}': {}", redact_key(k), e),
                        })?;
                        header_map.insert(name, value);
                    }
                    StreamableHttpClientTransportConfig::with_uri(url.as_str())
                        .custom_headers(header_map)
                };

                let transport = StreamableHttpClientTransport::from_config(config);

                let client_info = ClientInfo::new(
                    ClientCapabilities::default(),
                    Implementation::new("ironcrew", env!("CARGO_PKG_VERSION")),
                );

                let service =
                    client_info
                        .serve(transport)
                        .await
                        .map_err(|e| IronCrewError::Mcp {
                            server: cfg.label.clone(),
                            message: format!("HTTP handshake failed: {}", e),
                        })?;

                Ok(Self::from_service(service))
            }
        }
    }

    /// List all tools using paginated `list_all_tools()`.
    pub async fn list_all_tools(&self) -> Result<Vec<rmcp::model::Tool>> {
        self.peer
            .list_all_tools()
            .await
            .map_err(|e| IronCrewError::Mcp {
                server: String::new(),
                message: format!("list_all_tools failed: {}", e),
            })
    }

    /// Call a tool by its server-local name (not the prefixed IronCrew name).
    pub async fn call_tool(
        &self,
        name: &str,
        args: serde_json::Value,
    ) -> Result<rmcp::model::CallToolResult> {
        let params = if let Some(obj) = args.as_object() {
            CallToolRequestParams::new(name.to_string()).with_arguments(obj.clone())
        } else {
            CallToolRequestParams::new(name.to_string())
        };

        self.peer
            .call_tool(params)
            .await
            .map_err(|e| IronCrewError::Mcp {
                server: String::new(),
                message: format!("call_tool '{}' failed: {}", name, e),
            })
    }

    /// Graceful async shutdown — awaits the service loop's exit and drops
    /// the transport (reaps stdio children). Idempotent: a second call is
    /// a no-op. Called by `McpConnectionManager::shutdown`.
    #[allow(dead_code)]
    pub async fn shutdown(&self) {
        let f = self.shutdown.lock().await.take();
        if let Some(f) = f {
            f().await;
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
                handle.spawn(async move { f().await });
            }
            Err(_) => {
                // No runtime — drop the owned service on this thread.
                drop(f);
            }
        }
    }
}

/// Redact auth/sensitive header names when logging.
fn redact_key(key: &str) -> &str {
    let lower = key.to_ascii_lowercase();
    if matches!(
        lower.as_str(),
        "authorization"
            | "x-api-key"
            | "x-auth-token"
            | "cookie"
            | "proxy-authorization"
            | "set-cookie"
    ) {
        "[REDACTED]"
    } else {
        key
    }
}
