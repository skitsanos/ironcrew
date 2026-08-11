//! `McpBridgeTool` — bridges an MCP server tool into IronCrew's `Tool` trait.
//!
//! Tool results are size-capped at `IRONCREW_MCP_TOOL_RESULT_MAX_BYTES`
//! (default 256 KB). Oversized results are truncated with a marker appended.

use std::sync::Arc;

use async_trait::async_trait;

use crate::llm::provider::ToolSchema;
use crate::mcp::client::McpClient;
use crate::mcp::config::make_tool_name;
use crate::mcp::execution_policy::McpCallPolicy;
use crate::tools::{Tool, ToolCallContext};
use crate::utils::error::{IronCrewError, Result};

/// Default maximum tool result size (256 KB).
const DEFAULT_MAX_RESULT_BYTES: usize = 262_144;
const HARD_MAX_RESULT_BYTES: usize = 16 * 1024 * 1024;
const DEFAULT_MAX_CONTENT_ITEMS: usize = 256;
const HARD_MAX_CONTENT_ITEMS: usize = 4_096;
const DEFAULT_MAX_DESCRIPTION_BYTES: usize = 16 * 1024;
const DEFAULT_MAX_SCHEMA_BYTES: usize = 64 * 1024;
const HARD_MAX_DEFINITION_FIELD_BYTES: usize = 1024 * 1024;

fn bounded_env(name: &str, default: usize, hard_max: usize) -> Result<usize> {
    let value = match std::env::var(name) {
        Ok(raw) => raw.parse::<usize>().map_err(|_| IronCrewError::Mcp {
            server: String::new(),
            message: format!("{name} must be an integer from 1 to {hard_max}"),
        })?,
        Err(_) => default,
    };
    if !(1..=hard_max).contains(&value) {
        return Err(IronCrewError::Mcp {
            server: String::new(),
            message: format!("{name} must be from 1 to {hard_max}"),
        });
    }
    Ok(value)
}

fn utf8_prefix(value: &str, max_bytes: usize) -> &str {
    let mut end = value.len().min(max_bytes);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

fn append_bounded(output: &mut String, value: &str, cap: usize) {
    if output.len() >= cap {
        return;
    }
    output.push_str(utf8_prefix(value, cap - output.len()));
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
    execution_identity_fingerprint: Option<String>,
    call_policy: McpCallPolicy,
    result_max_bytes: usize,
    max_content_items: usize,
}

impl McpBridgeTool {
    /// Create a bridge tool from a raw rmcp `Tool` definition.
    pub fn from_rmcp_tool(
        server_label: &str,
        rmcp_tool: &rmcp::model::Tool,
        client: Arc<McpClient>,
        execution_identity_fingerprint: Option<String>,
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
        let max_description = bounded_env(
            "IRONCREW_MCP_MAX_TOOL_DESCRIPTION_BYTES",
            DEFAULT_MAX_DESCRIPTION_BYTES,
            HARD_MAX_DEFINITION_FIELD_BYTES,
        )?;
        if description.len() > max_description {
            return Err(IronCrewError::Mcp {
                server: server_label.to_string(),
                message: format!("MCP tool description exceeds {max_description} bytes"),
            });
        }

        // Convert rmcp's input_schema (serde_json::Map) to our ToolSchema parameters
        let parameters = serde_json::Value::Object(rmcp_tool.input_schema.as_ref().clone());
        let max_schema = bounded_env(
            "IRONCREW_MCP_MAX_TOOL_SCHEMA_BYTES",
            DEFAULT_MAX_SCHEMA_BYTES,
            HARD_MAX_DEFINITION_FIELD_BYTES,
        )?;
        let schema_bytes = serde_json::to_vec(&parameters)
            .map_err(|error| IronCrewError::Mcp {
                server: server_label.to_string(),
                message: format!("Failed to encode MCP tool schema: {error}"),
            })?
            .len();
        if schema_bytes > max_schema {
            return Err(IronCrewError::Mcp {
                server: server_label.to_string(),
                message: format!("MCP tool schema exceeds {max_schema} bytes"),
            });
        }

        let schema = ToolSchema {
            name: ironcrew_name.clone(),
            description: description.clone(),
            parameters,
        };
        let result_max_bytes = bounded_env(
            "IRONCREW_MCP_TOOL_RESULT_MAX_BYTES",
            DEFAULT_MAX_RESULT_BYTES,
            HARD_MAX_RESULT_BYTES,
        )?;
        let max_content_items = bounded_env(
            "IRONCREW_MCP_MAX_CONTENT_ITEMS",
            DEFAULT_MAX_CONTENT_ITEMS,
            HARD_MAX_CONTENT_ITEMS,
        )?;
        let call_policy = client.call_policy();

        Ok(Self {
            ironcrew_name,
            server_tool_name,
            description,
            schema,
            client,
            server_label: server_label.to_string(),
            execution_identity_fingerprint,
            call_policy,
            result_max_bytes,
            max_content_items,
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

    fn conversation_definition(&self) -> Result<serde_json::Value> {
        let identity = self
            .execution_identity_fingerprint
            .as_deref()
            .ok_or_else(|| {
                IronCrewError::Validation(format!(
                    "Persistent conversation tool '{}' requires a non-secret execution_identity for MCP server '{}'",
                    self.ironcrew_name, self.server_label
                ))
            })?;
        Ok(serde_json::json!({
            "schema": self.schema,
            "server_label": self.server_label,
            "server_tool_name": self.server_tool_name,
            "execution_identity_fingerprint": identity,
            "call_policy": self.call_policy.definition(),
            "result_max_bytes": self.result_max_bytes,
            "max_content_items": self.max_content_items,
        }))
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

        let cap = self.result_max_bytes;
        let max_items = self.max_content_items;
        if result.content.len() > max_items {
            return Err(IronCrewError::Mcp {
                server: self.server_label.clone(),
                message: format!(
                    "MCP tool returned {} content items; limit is {max_items}",
                    result.content.len()
                ),
            });
        }

        let mut output = String::with_capacity(cap.min(8 * 1024));
        let mut total_bytes = 0usize;
        let mut first = true;
        let is_error = result.is_error.unwrap_or(false);

        for content in &result.content {
            // rmcp v2 flattened the `Content { raw: RawContent }` wrapper into
            // a `ContentBlock` enum matched directly.
            let text = match content {
                rmcp::model::ContentBlock::Text(t) => t.text.as_str(),
                rmcp::model::ContentBlock::Image(_) => "[image content omitted]",
                rmcp::model::ContentBlock::Resource(r) => {
                    // Extract embedded text resources
                    if let rmcp::model::ResourceContents::TextResourceContents { text, .. } =
                        &r.resource
                    {
                        text.as_str()
                    } else {
                        "[binary resource omitted]"
                    }
                }
                rmcp::model::ContentBlock::Audio(_) => "[audio content omitted]",
                rmcp::model::ContentBlock::ResourceLink(_) => "[resource link omitted]",
                // ContentBlock is #[non_exhaustive] in rmcp v2 — tolerate any
                // future content type rather than dropping the tool result.
                _ => "[unsupported content omitted]",
            };
            if !first {
                total_bytes = total_bytes.saturating_add(1);
                append_bounded(&mut output, "\n", cap);
            }
            first = false;
            total_bytes = total_bytes.saturating_add(text.len());
            append_bounded(&mut output, text, cap);
        }

        if total_bytes > cap {
            let marker = format!("\n[truncated: result exceeded {cap}-byte limit]");
            let content_cap = cap.saturating_sub(marker.len());
            output.truncate(utf8_prefix(&output, content_cap).len());
            append_bounded(&mut output, &marker, cap);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf8_prefix_never_splits_multibyte_character() {
        let value = "a🙂b";
        assert_eq!(utf8_prefix(value, 1), "a");
        assert_eq!(utf8_prefix(value, 2), "a");
        assert_eq!(utf8_prefix(value, 5), "a🙂");
    }

    #[test]
    fn bounded_append_is_utf8_safe_and_never_exceeds_cap() {
        let mut output = String::new();
        append_bounded(&mut output, "🙂🙂", 5);
        assert_eq!(output, "🙂");
        assert!(output.len() <= 5);
    }
}
