use async_trait::async_trait;
use serde_json::json;
use tera::{Context, Tera};

use super::Tool;
use crate::llm::provider::ToolSchema;
use crate::utils::error::{IronCrewError, Result};

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

    async fn execute(&self, args: serde_json::Value) -> Result<String> {
        let template_str =
            args["template"]
                .as_str()
                .ok_or_else(|| IronCrewError::ToolExecution {
                    tool: "template_render".into(),
                    message: "Missing 'template' argument".into(),
                })?;

        let data = args.get("data").cloned().unwrap_or(json!({}));

        let mut tera = Tera::default();
        tera.add_raw_template("inline", template_str).map_err(|e| {
            IronCrewError::ToolExecution {
                tool: "template_render".into(),
                message: format!("Template parse error: {}", e),
            }
        })?;

        let context = Context::from_serialize(&data).map_err(|e| IronCrewError::ToolExecution {
            tool: "template_render".into(),
            message: format!("Context error: {}", e),
        })?;

        tera.render("inline", &context)
            .map_err(|e| IronCrewError::ToolExecution {
                tool: "template_render".into(),
                message: format!("Render error: {}", e),
            })
    }
}
