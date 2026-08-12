use async_trait::async_trait;
use serde_json::json;
use std::io::Write;
use tera::{Context, Tera};

use super::{Tool, ToolCallContext};
use crate::llm::provider::ToolSchema;
use crate::utils::error::{IronCrewError, Result};

const MAX_TEMPLATE_BYTES: usize = 1024 * 1024;
const MAX_DATA_BYTES: usize = 8 * 1024 * 1024;
const MAX_OUTPUT_BYTES: usize = 16 * 1024 * 1024;

struct BoundedOutput {
    bytes: Vec<u8>,
    overflowed: bool,
}

impl Write for BoundedOutput {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self.bytes.len().saturating_add(bytes.len()) > MAX_OUTPUT_BYTES {
            self.overflowed = true;
            return Err(std::io::Error::new(
                std::io::ErrorKind::FileTooLarge,
                format!("template output exceeds {MAX_OUTPUT_BYTES} bytes"),
            ));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[derive(Default)]
pub struct TemplateRenderTool;

impl TemplateRenderTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Tool for TemplateRenderTool {
    fn name(&self) -> &str {
        "template_render"
    }
    fn description(&self) -> &str {
        "Render a Tera template with JSON data"
    }
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "template_render".into(),
            description: self.description().into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "template": { "type": "string", "description": "Tera template string (e.g., 'Hello {{ name }}!')" },
                    "data": { "type": "object", "description": "JSON data to pass to the template as variables" }
                },
                "required": ["template", "data"]
            }),
        }
    }

    async fn execute(&self, args: serde_json::Value, _ctx: &ToolCallContext) -> Result<String> {
        let template_str = args["template"]
            .as_str()
            .ok_or_else(|| IronCrewError::ToolExecution {
                tool: "template_render".into(),
                message: "Missing 'template' argument".into(),
            })?
            .to_owned();

        if template_str.len() > MAX_TEMPLATE_BYTES {
            return Err(IronCrewError::ToolExecution {
                tool: "template_render".into(),
                message: format!("Template exceeds {MAX_TEMPLATE_BYTES} bytes"),
            });
        }

        let data = args.get("data").cloned().unwrap_or(json!({}));
        crate::utils::http::to_json_pretty_limited(&data, MAX_DATA_BYTES).map_err(|e| {
            IronCrewError::ToolExecution {
                tool: "template_render".into(),
                message: format!("Template data exceeds {MAX_DATA_BYTES} bytes: {e}"),
            }
        })?;

        // Tera is synchronous. Keep large-but-bounded template work off the
        // async runtime, and stream into a capped writer instead of building
        // an unbounded String before checking its size.
        tokio::task::spawn_blocking(move || {
            let mut tera = Tera::default();
            tera.add_raw_template("inline", &template_str)
                .map_err(|e| IronCrewError::ToolExecution {
                    tool: "template_render".into(),
                    message: format!("Template parse error: {e}"),
                })?;
            let context =
                Context::from_serialize(&data).map_err(|e| IronCrewError::ToolExecution {
                    tool: "template_render".into(),
                    message: format!("Context error: {e}"),
                })?;
            let mut output = BoundedOutput {
                bytes: Vec::with_capacity(4096),
                overflowed: false,
            };
            let render_result = tera.render_to("inline", &context, &mut output);
            if output.overflowed {
                return Err(IronCrewError::ToolExecution {
                    tool: "template_render".into(),
                    message: format!("template output exceeds {MAX_OUTPUT_BYTES} bytes"),
                });
            }
            render_result.map_err(|e| IronCrewError::ToolExecution {
                tool: "template_render".into(),
                message: format!("Render error: {e}"),
            })?;
            String::from_utf8(output.bytes).map_err(|e| IronCrewError::ToolExecution {
                tool: "template_render".into(),
                message: format!("Template emitted invalid UTF-8: {e}"),
            })
        })
        .await
        .map_err(|e| IronCrewError::ToolExecution {
            tool: "template_render".into(),
            message: format!("Template worker failed: {e}"),
        })?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn output_is_bounded_during_rendering() {
        let tool = TemplateRenderTool::new();
        let error = tool
            .execute(
                json!({
                    "template": "{{ chunk }}{{ chunk }}{{ chunk }}",
                    "data": { "chunk": "x".repeat(6 * 1024 * 1024) }
                }),
                &ToolCallContext::default(),
            )
            .await
            .expect_err("oversized output must fail");
        assert!(error.to_string().contains("template output exceeds"));
    }
}
