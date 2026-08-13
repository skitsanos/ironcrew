//! `McpConnectionManager` — manages a pool of MCP server connections.
//!
//! All servers from a crew's `mcp_servers` config are connected in parallel
//! at the first `crew:run()` call. A connection failure on any server aborts
//! the whole bounded batch after every connection attempt settles. The manager is then cached on `LuaCrew` so
//! subsequent runs reuse the same connections.

use std::collections::HashMap;
use std::sync::Arc;

use futures::future::join_all;

use crate::mcp::bridge::McpBridgeTool;
use crate::mcp::client::McpClient;
use crate::mcp::config::McpConfig;
use crate::tools::Tool;
use crate::tools::registry::ToolRegistry;
use crate::utils::error::{IronCrewError, Result};

// ── McpConnectionManager ──────────────────────────────────────────────────────

/// Holds live MCP client connections indexed by server label.
pub struct McpConnectionManager {
    /// `label → client` mapping; `Arc` so bridge tools can hold a reference.
    clients: HashMap<String, Arc<McpClient>>,
}

impl McpConnectionManager {
    /// Spawn all configured MCP servers in parallel.
    ///
    /// Each client enforces one transport-aware timeout from
    /// `IRONCREW_MCP_DISCOVERY_TIMEOUT_SECS` (default 10 s). A single failure
    /// returns an error after all bounded attempts settle and closes every
    /// connection that did succeed.
    ///
    /// After successful connection, all discovered tools are registered into
    /// `tool_registry` using the `mcp__<label>__<tool>` naming scheme.
    pub async fn connect_all(config: &McpConfig, tool_registry: &mut ToolRegistry) -> Result<Self> {
        // Build one connect future per server
        let connect_futs: Vec<_> = config
            .servers
            .iter()
            .map(|server_cfg| {
                let label = server_cfg.label.clone();
                let cfg = server_cfg.clone();

                async move {
                    tracing::info!(server = %label, "Connecting to MCP server");

                    let client = McpClient::connect(&cfg).await?;

                    tracing::info!(server = %label, "Connected to MCP server");
                    Ok::<(String, McpClient), IronCrewError>((label, client))
                }
            })
            .collect();

        // Run every bounded connection attempt, then fail the batch as one unit.
        let mut clients: HashMap<String, Arc<McpClient>> = HashMap::new();
        let mut connect_error = None;
        for result in join_all(connect_futs).await {
            match result {
                Ok((label, client)) => {
                    clients.insert(label, Arc::new(client));
                }
                Err(error) if connect_error.is_none() => connect_error = Some(error),
                Err(_) => {}
            }
        }
        if let Some(error) = connect_error {
            shutdown_clients(&clients).await;
            return Err(error);
        }

        // Finish discovery for every server before mutating the caller's
        // registry. A failure tears down all successfully connected peers.
        let mut discovered = Vec::with_capacity(clients.len());
        for (label, client) in &clients {
            let tools = match client.list_all_tools().await {
                Ok(tools) => tools,
                Err(IronCrewError::Mcp { message, .. }) => {
                    let error = IronCrewError::Mcp {
                        server: label.clone(),
                        message,
                    };
                    shutdown_clients(&clients).await;
                    return Err(error);
                }
                Err(error) => {
                    shutdown_clients(&clients).await;
                    return Err(error);
                }
            };
            discovered.push((label.clone(), Arc::clone(client), tools));
        }

        // Register tools only after every server completed bounded discovery.
        for (label, client, tools) in discovered {
            tracing::info!(
                server = %label,
                count = tools.len(),
                "Registering MCP tools"
            );

            for rmcp_tool in &tools {
                let execution_identity_fingerprint = config
                    .servers
                    .iter()
                    .find(|server| server.label == label)
                    .and_then(|server| server.execution_identity_fingerprint.clone());
                match McpBridgeTool::from_rmcp_tool(
                    &label,
                    rmcp_tool,
                    client.clone(),
                    execution_identity_fingerprint,
                ) {
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

async fn shutdown_clients(clients: &HashMap<String, Arc<McpClient>>) {
    join_all(clients.values().map(|client| client.shutdown())).await;
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
