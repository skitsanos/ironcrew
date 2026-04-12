//! `McpConnectionManager` — manages a pool of MCP server connections.
//!
//! All servers from a crew's `mcp_servers` config are connected in parallel
//! at the first `crew:run()` call. A connection failure on any server aborts
//! the whole batch (fail-fast). The manager is then cached on `LuaCrew` so
//! subsequent runs reuse the same connections.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use futures::future::try_join_all;

use crate::mcp::bridge::McpBridgeTool;
use crate::mcp::client::McpClient;
use crate::mcp::config::McpConfig;
use crate::tools::Tool;
use crate::tools::registry::ToolRegistry;
use crate::utils::error::{IronCrewError, Result};

// ── Handshake timeout ─────────────────────────────────────────────────────────

fn handshake_timeout() -> Duration {
    let secs = std::env::var("IRONCREW_MCP_HANDSHAKE_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(10);
    Duration::from_secs(secs)
}

// ── McpConnectionManager ──────────────────────────────────────────────────────

/// Holds live MCP client connections indexed by server label.
pub struct McpConnectionManager {
    /// `label → client` mapping; `Arc` so bridge tools can hold a reference.
    clients: HashMap<String, Arc<McpClient>>,
}

impl McpConnectionManager {
    /// Spawn all configured MCP servers in parallel.
    ///
    /// Each connection attempt is wrapped in a timeout of
    /// `IRONCREW_MCP_HANDSHAKE_TIMEOUT_SECS` (default 10 s). A single failure
    /// returns an error and no further servers are connected.
    ///
    /// After successful connection, all discovered tools are registered into
    /// `tool_registry` using the `mcp__<label>__<tool>` naming scheme.
    pub async fn connect_all(config: &McpConfig, tool_registry: &mut ToolRegistry) -> Result<Self> {
        let timeout = handshake_timeout();

        // Build one connect future per server
        let connect_futs: Vec<_> = config
            .servers
            .iter()
            .map(|server_cfg| {
                let label = server_cfg.label.clone();
                let cfg = server_cfg.clone();

                async move {
                    tracing::info!(server = %label, "Connecting to MCP server");

                    let client = tokio::time::timeout(timeout, McpClient::connect(&cfg))
                        .await
                        .map_err(|_| IronCrewError::Mcp {
                            server: label.clone(),
                            message: format!(
                                "Handshake timed out after {}s. \
                                 Adjust IRONCREW_MCP_HANDSHAKE_TIMEOUT_SECS.",
                                timeout.as_secs()
                            ),
                        })??;

                    tracing::info!(server = %label, "Connected to MCP server");
                    Ok::<(String, McpClient), IronCrewError>((label, client))
                }
            })
            .collect();

        // Parallel connect — fail fast on first error
        let connected: Vec<(String, McpClient)> = try_join_all(connect_futs).await?;

        // Wrap in Arc and collect
        let mut clients: HashMap<String, Arc<McpClient>> = HashMap::new();
        for (label, client) in connected {
            clients.insert(label, Arc::new(client));
        }

        // Register all tools
        for (label, client) in &clients {
            let tools = client.list_all_tools().await.map_err(|e| {
                if let IronCrewError::Mcp { message, .. } = e {
                    IronCrewError::Mcp {
                        server: label.clone(),
                        message,
                    }
                } else {
                    e
                }
            })?;

            tracing::info!(
                server = %label,
                count = tools.len(),
                "Registering MCP tools"
            );

            for rmcp_tool in &tools {
                match McpBridgeTool::from_rmcp_tool(label, rmcp_tool, client.clone()) {
                    Ok(bridge) => {
                        tracing::debug!(
                            server = %label,
                            tool = bridge.name(),
                            "Registered MCP bridge tool"
                        );
                        tool_registry.register(Box::new(bridge));
                    }
                    Err(e) => {
                        tracing::warn!(
                            server = %label,
                            tool = %rmcp_tool.name,
                            error = %e,
                            "Skipping MCP tool due to name validation failure"
                        );
                    }
                }
            }
        }

        Ok(Self { clients })
    }

    /// Returns the number of connected servers.
    #[allow(dead_code)]
    pub fn server_count(&self) -> usize {
        self.clients.len()
    }

    /// Deterministic async shutdown. Awaits each client's service loop
    /// exit so stdio children are reaped and memory is freed before
    /// returning. Use this from graceful-shutdown paths (SIGTERM handler,
    /// CLI `run` completion). Safe to call multiple times.
    #[allow(dead_code)]
    pub async fn shutdown(&self) {
        // Shut down clients in parallel — they are independent.
        let futs = self.clients.values().map(|c| c.shutdown());
        futures::future::join_all(futs).await;
    }
}

impl Drop for McpConnectionManager {
    /// Best-effort shutdown for unexpected drops. Spawns each client's
    /// async shutdown on the current runtime. Prefer calling
    /// `shutdown().await` explicitly for deterministic cleanup.
    fn drop(&mut self) {
        for client in self.clients.values() {
            client.shutdown_blocking();
        }
    }
}
