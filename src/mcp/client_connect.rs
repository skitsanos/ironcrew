//! Strict MCP 2026 transport construction and discovery.

#[cfg(unix)]
use std::collections::HashMap;

use rmcp::{
    ClientServiceExt, RoleClient,
    service::RunningService,
    transport::{
        StreamableHttpClientTransport, common::client_side_sse::NeverRetry,
        streamable_http_client::StreamableHttpClientTransportConfig,
    },
};
#[cfg(unix)]
use tokio::process::Command;

#[cfg(unix)]
use super::stdio_transport::StrictStdioTransport;
use super::{
    client::{McpClient, configured_timeout},
    config::{McpServerConfig, McpTransportConfig},
    connection::{DiscoveryGuard, PoisonSignal},
    execution_policy::McpCallPolicy,
    http_headers::configured_header_map,
    http_tool_headers::HttpToolHeaderRegistry,
    http_transport::Strict2026HttpClient,
    lifecycle::{StrictClientHandler, discovery_lifecycle},
};
use crate::{
    utils::error::{IronCrewError, Result},
    utils::network::{OutboundNetworkPolicy, secure_no_redirect_client},
};

const DEFAULT_DISCOVERY_TIMEOUT_SECS: u64 = 10;

#[cfg(unix)]
const SAFE_ENV_KEYS: &[&str] = &["PATH", "HOME", "USER", "LANG"];

#[cfg(unix)]
fn build_child_env(config_env: &HashMap<String, String>, inherit: bool) -> HashMap<String, String> {
    let mut env: HashMap<String, String> = if inherit {
        std::env::vars().collect()
    } else {
        SAFE_ENV_KEYS
            .iter()
            .filter_map(|key| std::env::var(key).ok().map(|value| ((*key).into(), value)))
            .chain(std::env::vars().filter(|(key, _)| key.starts_with("LC_")))
            .collect()
    };
    env.extend(config_env.clone());
    env
}

fn localhost_override_enabled() -> bool {
    std::env::var("IRONCREW_MCP_ALLOW_LOCALHOST")
        .map(|value| value == "1" || value.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

async fn require_tools_capability<S>(
    service: RunningService<RoleClient, S>,
    label: &str,
) -> Result<RunningService<RoleClient, S>>
where
    S: rmcp::Service<RoleClient> + 'static,
{
    if service
        .peer()
        .peer_info()
        .is_some_and(|info| info.capabilities.tools.is_some())
    {
        return Ok(service);
    }
    if let Err(error) = service.cancel().await {
        tracing::debug!(%error, "MCP cleanup after missing tools capability failed");
    }
    Err(IronCrewError::Mcp {
        server: label.to_owned(),
        message: "MCP 2026 server did not declare the required tools capability".to_owned(),
    })
}

impl McpClient {
    /// Connect using a strict MCP 2026 transport.
    pub async fn connect(cfg: &McpServerConfig) -> Result<Self> {
        let call_policy = McpCallPolicy::capture()?;
        let (poison_signal, poison_watch) = PoisonSignal::channel();
        let timeout = configured_timeout(
            "IRONCREW_MCP_DISCOVERY_TIMEOUT_SECS",
            DEFAULT_DISCOVERY_TIMEOUT_SECS,
        )?;
        match &cfg.transport {
            McpTransportConfig::Stdio { command, args, env } => {
                #[cfg(not(unix))]
                {
                    let _ = (command, args, env);
                    Err(IronCrewError::Mcp {
                        server: cfg.label.clone(),
                        message: "strict stdio MCP requires Unix process-group ownership; use HTTP on this platform".to_owned(),
                    })
                }
                #[cfg(unix)]
                {
                    let mut command_line = Command::new(command);
                    command_line.args(args).env_clear();
                    for (key, value) in build_child_env(env, cfg.inherit_env) {
                        command_line.env(key, value);
                    }
                    let (transport, abort) = StrictStdioTransport::spawn(
                        &mut command_line,
                        call_policy.inbound_message_max_bytes(),
                        poison_watch,
                    )
                    .map_err(|error| IronCrewError::Mcp {
                        server: cfg.label.clone(),
                        message: format!("Failed to create stdio transport: {error}"),
                    })?;
                    let guard = DiscoveryGuard::new(poison_signal.clone(), Some(abort.clone()));
                    let service = guard
                        .run(
                            timeout,
                            &cfg.label,
                            "stdio",
                            StrictClientHandler
                                .serve_with_lifecycle(transport, discovery_lifecycle()),
                        )
                        .await?;
                    let service = require_tools_capability(service, &cfg.label).await?;
                    Ok(Self::from_service(
                        service,
                        poison_signal,
                        Some(abort),
                        call_policy,
                        None,
                    ))
                }
            }
            McpTransportConfig::Http { url, headers } => {
                let mut config = StreamableHttpClientTransportConfig::with_uri(url.as_str());
                if !headers.is_empty() {
                    config = config.custom_headers(configured_header_map(headers, &cfg.label)?);
                }
                config.retry_config = std::sync::Arc::new(NeverRetry::default());
                config.max_sse_event_size = call_policy.inbound_message_max_bytes();
                let network = if localhost_override_enabled() {
                    OutboundNetworkPolicy::AllowLoopback
                } else {
                    OutboundNetworkPolicy::PublicOnly
                };
                let inner =
                    secure_no_redirect_client(network).map_err(|error| IronCrewError::Mcp {
                        server: cfg.label.clone(),
                        message: format!("Failed to build safe HTTP client: {error}"),
                    })?;
                let headers = HttpToolHeaderRegistry::new();
                let client = Strict2026HttpClient::new(
                    inner,
                    call_policy.inbound_message_max_bytes(),
                    poison_watch,
                    headers.clone(),
                );
                let transport = StreamableHttpClientTransport::with_client(client, config);
                let guard = DiscoveryGuard::new(poison_signal.clone(), None);
                let service = guard
                    .run(
                        timeout,
                        &cfg.label,
                        "HTTP",
                        StrictClientHandler.serve_with_lifecycle(transport, discovery_lifecycle()),
                    )
                    .await?;
                let service = require_tools_capability(service, &cfg.label).await?;
                Ok(Self::from_service(
                    service,
                    poison_signal,
                    None,
                    call_policy,
                    Some(headers),
                ))
            }
        }
    }
}
