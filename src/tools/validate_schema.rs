use async_trait::async_trait;
use serde_json::json;

use super::Tool;
use crate::llm::provider::ToolSchema;
use crate::utils::error::{IronCrewError, Result};

#[derive(Default)]
pub struct ValidateSchemaTool;

impl ValidateSchemaTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Tool for ValidateSchemaTool {
    fn name(&self) -> &str {
        "validate_schema"
    }

    fn description(&self) -> &str {
        "Validate a JSON string against a JSON Schema and return validation results"
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "validate_schema".into(),
            description: self.description().into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "data": {
                        "type": "string",
                        "description": "JSON string to validate"
                    },
                    "schema": {
                        "type": "object",
                        "description": "JSON Schema to validate against"
                    }
                },
                "required": ["data", "schema"]
            }),
        }
    }

    async fn execute(&self, args: serde_json::Value) -> Result<String> {
        let data_str = args["data"]
            .as_str()
            .ok_or_else(|| IronCrewError::ToolExecution {
                tool: "validate_schema".into(),
                message: "Missing 'data' argument".into(),
            })?;

        let schema_value = args
            .get("schema")
            .ok_or_else(|| IronCrewError::ToolExecution {
                tool: "validate_schema".into(),
                message: "Missing 'schema' argument".into(),
            })?;

        // Parse the data
        let data: serde_json::Value =
            serde_json::from_str(data_str).map_err(|e| IronCrewError::ToolExecution {
                tool: "validate_schema".into(),
                message: format!("Invalid JSON data: {}", e),
            })?;

        // Compile the schema
        let validator =
            jsonschema::draft7::new(schema_value).map_err(|e| IronCrewError::ToolExecution {
                tool: "validate_schema".into(),
                message: format!("Invalid JSON Schema: {}", e),
            })?;

        // Validate
        match validator.validate(&data) {
            Ok(()) => Ok(serde_json::to_string_pretty(&json!({
                "valid": true,
                "errors": []
            }))
            .unwrap()),
            Err(first_error) => {
                // Collect all errors via iter_errors
                let error_list: Vec<serde_json::Value> = std::iter::once(first_error)
                    .map(|e| {
                        json!({
                            "path": e.instance_path().to_string(),
                            "message": e.to_string(),
                        })
                    })
                    .chain(validator.iter_errors(&data).skip(1).map(|e| {
                        json!({
                            "path": e.instance_path().to_string(),
                            "message": e.to_string(),
                        })
                    }))
                    .collect();

                Ok(serde_json::to_string_pretty(&json!({
                    "valid": false,
                    "error_count": error_list.len(),
                    "errors": error_list
                }))
                .unwrap())
            }
        }
    }
}
