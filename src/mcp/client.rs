//! Transport-agnostic MCP client wrapper.
//!
//! Wraps either a stdio or HTTP rmcp `RunningService` behind a uniform API
//! so the rest of IronCrew never needs to deal with transport generics.

use axum::http::{HeaderName, HeaderValue};
use futures::future::BoxFuture;
use rmcp::{
    ClientServiceExt, Peer, RoleClient,
    model::{CallToolRequestParams, PaginatedRequestParams},
    service::RunningService,
    transport::{
        StreamableHttpClientTransport, common::client_side_sse::NeverRetry,
        streamable_http_client::StreamableHttpClientTransportConfig,
    },
};
use std::collections::HashMap;
use std::time::Duration;
use tokio::process::Command;
use tokio::sync::Mutex;
use tokio::time::Instant;

use crate::mcp::config::{McpServerConfig, McpTransportConfig};
use crate::mcp::connection::{ConnectionPoison, InFlightGuard, PoisonSignal};
use crate::mcp::execution_policy::{McpCallPolicy, ensure_serialized_size};
use crate::mcp::http_transport::Strict2026HttpClient;
use crate::mcp::lifecycle::{StrictClientHandler, discovery_lifecycle};
use crate::mcp::stdio_transport::StrictStdioTransport;
use crate::utils::error::{IronCrewError, Result};
use crate::utils::network::{OutboundNetworkPolicy, secure_no_redirect_client};

const DEFAULT_DISCOVERY_TIMEOUT_SECS: u64 = 10;
const DEFAULT_LIST_TIMEOUT_SECS: u64 = 10;
const DEFAULT_SHUTDOWN_TIMEOUT_SECS: u64 = 5;
const MAX_TIMEOUT_SECS: u64 = 3_600;

const DEFAULT_MAX_TOOLS: usize = 128;
const HARD_MAX_TOOLS: usize = 4_096;
const DEFAULT_MAX_LIST_PAGES: usize = 32;
const HARD_MAX_LIST_PAGES: usize = 256;
const DEFAULT_MAX_TOOL_DEFINITION_BYTES: usize = 128 * 1024;
const HARD_MAX_TOOL_DEFINITION_BYTES: usize = 1024 * 1024;
const HARD_MAX_TOOL_NAME_BYTES: usize = 256;

fn mcp_error(message: impl Into<String>) -> IronCrewError {
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

fn bounded_env_usize(name: &str, default: usize, max: usize) -> Result<usize> {
    let value = match std::env::var(name) {
        Ok(raw) => raw
            .parse::<usize>()
            .map_err(|_| mcp_error(format!("{name} must be an integer from 1 to {max}")))?,
        Err(_) => default,
    };
    if !(1..=max).contains(&value) {
        return Err(mcp_error(format!("{name} must be from 1 to {max}")));
    }
    Ok(value)
}

fn configured_timeout(name: &str, default: u64) -> Result<Duration> {
    Ok(Duration::from_secs(bounded_env_u64(
        name,
        default,
        MAX_TIMEOUT_SECS,
    )?))
}

fn localhost_override_enabled() -> bool {
    std::env::var("IRONCREW_MCP_ALLOW_LOCALHOST")
        .map(|value| value == "1" || value.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

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
    call_policy: McpCallPolicy,
    poison: ConnectionPoison,
}

impl McpClient {
    fn from_service<S>(
        service: RunningService<RoleClient, S>,
        poison_signal: PoisonSignal,
        stdio_abort: Option<crate::mcp::stdio_transport::StdioAbortHandle>,
        call_policy: McpCallPolicy,
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
        }
    }

    /// Connect using a `McpServerConfig`, respecting all security constraints.
    pub async fn connect(cfg: &McpServerConfig) -> Result<Self> {
        let call_policy = McpCallPolicy::capture()?;
        let (poison_signal, poison_watch) = PoisonSignal::channel();
        let discovery_timeout = configured_timeout(
            "IRONCREW_MCP_DISCOVERY_TIMEOUT_SECS",
            DEFAULT_DISCOVERY_TIMEOUT_SECS,
        )?;
        match &cfg.transport {
            McpTransportConfig::Stdio { command, args, env } => {
                let child_env = build_child_env(env, cfg.inherit_env);

                let mut cmd = Command::new(command);
                cmd.args(args);
                // Replace the environment entirely with the curated set.
                cmd.env_clear();
                for (key, value) in &child_env {
                    cmd.env(key, value);
                }
                let (transport, stdio_abort) =
                    StrictStdioTransport::spawn(&mut cmd, call_policy.inbound_message_max_bytes())
                        .map_err(|e| IronCrewError::Mcp {
                            server: cfg.label.clone(),
                            message: format!("Failed to create stdio transport: {}", e),
                        })?;

                let service = tokio::time::timeout(
                    discovery_timeout,
                    StrictClientHandler.serve_with_lifecycle(transport, discovery_lifecycle()),
                )
                .await
                .map_err(|_| IronCrewError::Mcp {
                    server: cfg.label.clone(),
                    message: format!(
                        "MCP discovery timed out after {} seconds",
                        discovery_timeout.as_secs()
                    ),
                })?
                .map_err(|e| IronCrewError::Mcp {
                    server: cfg.label.clone(),
                    message: format!("MCP discovery failed: {}", e),
                })?;

                Ok(Self::from_service(
                    service,
                    poison_signal,
                    Some(stdio_abort),
                    call_policy,
                ))
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

                let policy = if localhost_override_enabled() {
                    OutboundNetworkPolicy::AllowLoopback
                } else {
                    OutboundNetworkPolicy::PublicOnly
                };
                let http_client =
                    secure_no_redirect_client(policy).map_err(|e| IronCrewError::Mcp {
                        server: cfg.label.clone(),
                        message: format!("Failed to build safe HTTP client: {e}"),
                    })?;
                let http_client = Strict2026HttpClient::new(
                    http_client,
                    call_policy.inbound_message_max_bytes(),
                    poison_watch,
                );
                let mut config = config;
                config.retry_config = std::sync::Arc::new(NeverRetry::default());
                config.max_sse_event_size = call_policy.inbound_message_max_bytes();
                let transport = StreamableHttpClientTransport::with_client(http_client, config);

                let service = tokio::time::timeout(
                    discovery_timeout,
                    StrictClientHandler.serve_with_lifecycle(transport, discovery_lifecycle()),
                )
                .await
                .map_err(|_| IronCrewError::Mcp {
                    server: cfg.label.clone(),
                    message: format!(
                        "MCP HTTP discovery timed out after {} seconds",
                        discovery_timeout.as_secs()
                    ),
                })?
                .map_err(|e| IronCrewError::Mcp {
                    server: cfg.label.clone(),
                    message: format!("MCP HTTP discovery failed: {}", e),
                })?;

                Ok(Self::from_service(
                    service,
                    poison_signal,
                    None,
                    call_policy,
                ))
            }
        }
    }

    /// List all tools using paginated `list_all_tools()`.
    pub async fn list_all_tools(&self) -> Result<Vec<rmcp::model::Tool>> {
        let timeout =
            configured_timeout("IRONCREW_MCP_LIST_TIMEOUT_SECS", DEFAULT_LIST_TIMEOUT_SECS)?;
        let max_tools =
            bounded_env_usize("IRONCREW_MCP_MAX_TOOLS", DEFAULT_MAX_TOOLS, HARD_MAX_TOOLS)?;
        let max_pages = bounded_env_usize(
            "IRONCREW_MCP_MAX_LIST_PAGES",
            DEFAULT_MAX_LIST_PAGES,
            HARD_MAX_LIST_PAGES,
        )?;
        let max_definition_bytes = bounded_env_usize(
            "IRONCREW_MCP_MAX_TOOL_DEFINITION_BYTES",
            DEFAULT_MAX_TOOL_DEFINITION_BYTES,
            HARD_MAX_TOOL_DEFINITION_BYTES,
        )?;

        let guard = InFlightGuard::new(self.poison.clone());
        let deadline = Instant::now() + timeout;
        let result = async {
            let mut tools = Vec::new();
            let mut cursor = None;
            let mut seen_cursors = std::collections::HashSet::new();

            for _ in 0..max_pages {
                let result = crate::mcp::call::list_tools(
                    &self.peer,
                    Some(PaginatedRequestParams::default().with_cursor(cursor.clone())),
                    "MCP tool discovery",
                    deadline,
                    &guard,
                )
                .await?;

                if tools.len().saturating_add(result.tools.len()) > max_tools {
                    return Err(mcp_error(format!(
                        "MCP server advertised more than {max_tools} tools"
                    )));
                }
                for tool in &result.tools {
                    ensure_serialized_size(tool, max_definition_bytes, "MCP tool definition")?;
                }
                tools.extend(result.tools);

                let Some(next) = result.next_cursor else {
                    return Ok(tools);
                };
                if !seen_cursors.insert(next.clone()) {
                    return Err(mcp_error("MCP tool pagination repeated a cursor"));
                }
                cursor = Some(next);
            }

            Err(mcp_error(format!(
                "MCP tool discovery exceeded {max_pages} pages"
            )))
        }
        .await;
        self.finish_in_flight(guard, result).await
    }

    /// Call a tool by its server-local name (not the prefixed IronCrew name).
    pub async fn call_tool(
        &self,
        name: &str,
        args: serde_json::Value,
    ) -> Result<rmcp::model::CallToolResult> {
        if name.is_empty() || name.len() > HARD_MAX_TOOL_NAME_BYTES {
            return Err(mcp_error(format!(
                "MCP tool name must contain 1..={HARD_MAX_TOOL_NAME_BYTES} bytes"
            )));
        }
        self.call_policy.validate_arguments(&args)?;

        let params = match args {
            serde_json::Value::Object(arguments) => {
                CallToolRequestParams::new(name.to_string()).with_arguments(arguments)
            }
            serde_json::Value::Null => CallToolRequestParams::new(name.to_string()),
            _ => return Err(mcp_error("MCP tool arguments must be a JSON object")),
        };

        let guard = InFlightGuard::new(self.poison.clone());
        let result =
            crate::mcp::call::call_tool(&self.peer, params, name, self.call_policy, &guard).await;
        self.finish_in_flight(guard, result).await
    }

    async fn finish_in_flight<T>(&self, guard: InFlightGuard, result: Result<T>) -> Result<T> {
        if self.poison.is_poisoned() {
            self.shutdown().await;
        }
        guard.disarm();
        result
    }

    pub(super) fn call_policy(&self) -> McpCallPolicy {
        self.call_policy
    }

    /// Graceful async shutdown — awaits the service loop's exit and drops
    /// the transport (reaps stdio children). Idempotent: a second call is
    /// a no-op. Called by `McpConnectionManager::shutdown`.
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
