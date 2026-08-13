//! Serialized, bounded MCP tool discovery and invocation operations.

use std::collections::HashSet;

use rmcp::model::{CallToolRequestParams, PaginatedRequestParams, Tool};
use tokio::time::Instant;

use super::{
    call::{self, CallToolFailure},
    client::{McpClient, configured_timeout, mcp_error},
    connection::InFlightGuard,
    execution_policy::ensure_serialized_size,
};
use crate::utils::error::Result;

const DEFAULT_LIST_TIMEOUT_SECS: u64 = 10;
const DEFAULT_MAX_TOOLS: usize = 128;
const HARD_MAX_TOOLS: usize = 4_096;
const DEFAULT_MAX_LIST_PAGES: usize = 32;
const HARD_MAX_LIST_PAGES: usize = 256;
const DEFAULT_MAX_TOOL_DEFINITION_BYTES: usize = 128 * 1024;
const HARD_MAX_TOOL_DEFINITION_BYTES: usize = 1024 * 1024;
const HARD_MAX_TOOL_NAME_BYTES: usize = 256;

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

impl McpClient {
    /// List all tools using bounded pagination.
    pub async fn list_all_tools(&self) -> Result<Vec<Tool>> {
        let timeout =
            configured_timeout("IRONCREW_MCP_LIST_TIMEOUT_SECS", DEFAULT_LIST_TIMEOUT_SECS)?;
        let deadline = Instant::now() + timeout;
        let _operation = tokio::time::timeout_at(deadline, self.operation.lock())
            .await
            .map_err(|_| mcp_error("MCP tool discovery reached its configured deadline"))?;
        let guard = InFlightGuard::new(self.poison.clone());
        let result = self.list_tools_with_guard(&guard, deadline, false).await;
        self.finish_in_flight(guard, result).await
    }

    async fn list_tools_with_guard(
        &self,
        guard: &InFlightGuard,
        deadline: Instant,
        refresh: bool,
    ) -> Result<Vec<Tool>> {
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
        let mut listing = self
            .tool_headers
            .as_ref()
            .map(|registry| registry.begin_listing())
            .transpose()
            .map_err(|error| mcp_error(error.to_string()))?;
        let mut tools = Vec::new();
        let mut cursor = None;
        let mut seen_cursors = HashSet::new();

        for _ in 0..max_pages {
            let mut result = call::list_tools(
                &self.peer,
                Some(PaginatedRequestParams::default().with_cursor(cursor.clone())),
                "MCP tool discovery",
                deadline,
                guard,
            )
            .await?;
            if let Some(listing) = &mut listing {
                listing
                    .restore_page(&mut result.tools)
                    .map_err(|error| mcp_error(error.to_string()))?;
            }
            if tools.len().saturating_add(result.tools.len()) > max_tools {
                guard.poison();
                return Err(mcp_error(format!(
                    "MCP server advertised more than {max_tools} tools"
                )));
            }
            for tool in &result.tools {
                if let Err(error) =
                    ensure_serialized_size(tool, max_definition_bytes, "MCP tool definition")
                {
                    guard.poison();
                    return Err(error);
                }
            }
            tools.extend(result.tools);

            let Some(next) = result.next_cursor else {
                if let Some(listing) = listing.take() {
                    let validated = listing
                        .into_validated()
                        .map_err(|error| mcp_error(error.to_string()))?;
                    if refresh {
                        validated
                            .commit_refresh()
                            .map_err(|error| mcp_error(error.to_string()))?;
                    } else {
                        validated
                            .commit()
                            .map_err(|error| mcp_error(error.to_string()))?;
                    }
                }
                return Ok(tools);
            };
            if !seen_cursors.insert(next.clone()) {
                guard.poison();
                return Err(mcp_error("MCP tool pagination repeated a cursor"));
            }
            cursor = Some(next);
        }

        guard.poison();
        Err(mcp_error(format!(
            "MCP tool discovery exceeded {max_pages} pages"
        )))
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
        let mut params = match args {
            serde_json::Value::Object(arguments) => {
                CallToolRequestParams::new(name.to_string()).with_arguments(arguments)
            }
            serde_json::Value::Null => CallToolRequestParams::new(name.to_string()),
            _ => return Err(mcp_error("MCP tool arguments must be a JSON object")),
        };

        let deadline = Instant::now() + self.call_policy.timeout();
        let _operation = tokio::time::timeout_at(deadline, self.operation.lock())
            .await
            .map_err(|_| {
                mcp_error(format!(
                    "call_tool '{name}' reached its configured deadline"
                ))
            })?;
        let guard = InFlightGuard::new(self.poison.clone());
        let mut attempts = 0;
        let first = call::call_tool(
            &self.peer,
            &mut params,
            name,
            self.call_policy,
            deadline,
            &mut attempts,
            &guard,
        )
        .await;
        let result = match first {
            Ok(result) => Ok(result),
            Err(CallToolFailure::HeaderMismatch(_)) if self.tool_headers.is_some() => {
                match self.list_tools_with_guard(&guard, deadline, true).await {
                    Ok(tools) if tools.iter().any(|tool| tool.name.as_ref() == name) => {
                        call::call_tool(
                            &self.peer,
                            &mut params,
                            name,
                            self.call_policy,
                            deadline,
                            &mut attempts,
                            &guard,
                        )
                        .await
                        .map_err(|error| {
                            guard.poison();
                            error.into_error()
                        })
                    }
                    Ok(_) => {
                        guard.poison();
                        Err(mcp_error(format!(
                            "MCP tool `{name}` was absent after parameter-header refresh"
                        )))
                    }
                    Err(error) => {
                        guard.poison();
                        Err(error)
                    }
                }
            }
            Err(error) => Err(error.into_error()),
        };
        self.finish_in_flight(guard, result).await
    }

    async fn finish_in_flight<T>(&self, guard: InFlightGuard, result: Result<T>) -> Result<T> {
        if self.poison.is_poisoned() {
            self.shutdown().await;
        }
        guard.disarm();
        result
    }
}
