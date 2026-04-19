//! `McpBridgeTool` — bridges an MCP server tool into IronCrew's `Tool` trait.
//!
//! Tool results are size-capped at `IRONCREW_MCP_TOOL_RESULT_MAX_BYTES`
//! (default 256 KB). Oversized results are truncated with a marker appended.

use std::sync::Arc;

use async_trait::async_trait;

use crate::llm::provider::ToolSchema;
use crate::mcp::client::McpClient;
use crate::mcp::config::make_tool_name;
use crate::tools::{Tool, ToolCallContext};
use crate::utils::error::{IronCrewError, Result};

/// Default maximum tool result size (256 KB).
const DEFAULT_MAX_RESULT_BYTES: usize = 262_144;

fn max_result_bytes() -> usize {
    std::env::var("IRONCREW_MCP_TOOL_RESULT_MAX_BYTES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_MAX_RESULT_BYTES)
}

/// A tool registered in IronCrew's registry that proxies calls to an MCP server.
pub struct McpBridgeTool {
    /// Full IronCrew tool name, e.g. `mcp__git__git_status`.
    ironcrew_name: String,
    /// Original server-local tool name, e.g. `git_status`.
    server_tool_name: String,
    description: String,
    schema: ToolSchema,
    client: Arc<McpClient>,
    server_label: String,
}

impl McpBridgeTool {
    /// Create a bridge tool from a raw rmcp `Tool` definition.
    pub fn from_rmcp_tool(
        server_label: &str,
        rmcp_tool: &rmcp::model::Tool,
        client: Arc<McpClient>,
    ) -> Result<Self> {
        let server_tool_name = rmcp_tool.name.to_string();

        let ironcrew_name =
            make_tool_name(server_label, &server_tool_name).map_err(|e| IronCrewError::Mcp {
                server: server_label.to_string(),
                message: e,
            })?;

        let description = rmcp_tool
            .description
            .as_deref()
            .unwrap_or("(no description)")
            .to_string();

        // Convert rmcp's input_schema (serde_json::Map) to our ToolSchema parameters
        let parameters = serde_json::Value::Object(rmcp_tool.input_schema.as_ref().clone());

        let schema = ToolSchema {
            name: ironcrew_name.clone(),
            description: description.clone(),
            parameters,
        };

        Ok(Self {
            ironcrew_name,
            server_tool_name,
            description,
            schema,
            client,
            server_label: server_label.to_string(),
        })
    }
}

#[async_trait]
impl Tool for McpBridgeTool {
    fn name(&self) -> &str {
        &self.ironcrew_name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn schema(&self) -> ToolSchema {
        self.schema.clone()
    }

    async fn execute(&self, args: serde_json::Value, _ctx: &ToolCallContext) -> Result<String> {
        let result = self
            .client
            .call_tool(&self.server_tool_name, args)
            .await
            .map_err(|e| {
                // Enrich the error with the server label
                if let IronCrewError::Mcp { message, .. } = e {
                    IronCrewError::Mcp {
                        server: self.server_label.clone(),
                        message,
                    }
                } else {
                    e
                }
            })?;

        // Collect text from all Content::Text items
        let mut parts: Vec<String> = Vec::new();
        let is_error = result.is_error.unwrap_or(false);

        for content in &result.content {
            match &content.raw {
                rmcp::model::RawContent::Text(t) => {
                    parts.push(t.text.clone());
                }
                rmcp::model::RawContent::Image(_) => {
                    parts.push("[image content omitted]".to_string());
                }
                rmcp::model::RawContent::Resource(r) => {
                    // Extract embedded text resources
                    if let rmcp::model::ResourceContents::TextResourceContents { text, .. } =
                        &r.resource
                    {
                        parts.push(text.clone());
                    }
                }
                rmcp::model::RawContent::Audio(_) => {
                    parts.push("[audio content omitted]".to_string());
                }
                rmcp::model::RawContent::ResourceLink(_) => {
                    parts.push("[resource link omitted]".to_string());
                }
            }
        }

        let mut output = parts.join("\n");

        // Apply byte cap
        let cap = max_result_bytes();
        let byte_len = output.len();
        if byte_len > cap {
            // Truncate at a character boundary
            let mut truncated = output[..cap].to_string();
            truncated.push_str(&format!("\n[truncated: {} bytes omitted]", byte_len - cap));
            output = truncated;
        }

        if is_error {
            Err(IronCrewError::Mcp {
                server: self.server_label.clone(),
                message: output,
            })
        } else {
            Ok(output)
        }
    }
}
