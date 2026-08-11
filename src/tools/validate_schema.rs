use async_trait::async_trait;
use serde_json::json;

use super::{Tool, ToolCallContext};
use crate::llm::provider::ToolSchema;
use crate::utils::error::{IronCrewError, Result};

const DEFAULT_MAX_SCHEMA_BYTES: usize = 256 * 1024;
const HARD_MAX_SCHEMA_BYTES: usize = 4 * 1024 * 1024;
const MAX_DATA_BYTES: usize = 4 * 1024 * 1024;
const MAX_VALIDATION_ERRORS: usize = 100;
const MAX_ERROR_FIELD_BYTES: usize = 4096;
const MAX_RESULT_BYTES: usize = 1024 * 1024;

/// Compile a Draft 7 schema without permitting external document retrieval.
///
/// `jsonschema` is built without its HTTP/file retrieval features, and this
/// explicit walk rejects every non-fragment `$ref` before compilation. Local
/// references such as `#/definitions/item` remain supported.
pub fn compile_local_draft7(
    schema: &serde_json::Value,
) -> std::result::Result<jsonschema::Validator, String> {
    let max_bytes = schema_limit_from_env()?;
    compile_local_draft7_with_limit(schema, max_bytes)
}

fn schema_limit_from_env() -> std::result::Result<usize, String> {
    super::execution_policy::strict_env_usize(
        "IRONCREW_JSON_SCHEMA_MAX_BYTES",
        DEFAULT_MAX_SCHEMA_BYTES,
        1024,
        HARD_MAX_SCHEMA_BYTES,
    )
}

fn compile_local_draft7_with_limit(
    schema: &serde_json::Value,
    max_bytes: usize,
) -> std::result::Result<jsonschema::Validator, String> {
    crate::utils::http::to_json_pretty_limited(schema, max_bytes)
        .map_err(|error| format!("JSON Schema exceeds {max_bytes} bytes: {error}"))?;

    reject_external_refs(schema)?;
    jsonschema::draft7::new(schema).map_err(|error| error.to_string())
}

fn reject_external_refs(value: &serde_json::Value) -> std::result::Result<(), String> {
    match value {
        serde_json::Value::Object(object) => {
            if let Some(reference) = object.get("$ref").and_then(serde_json::Value::as_str)
                && !reference.starts_with('#')
            {
                return Err(
                    "External JSON Schema $ref values are disabled; use a local # fragment"
                        .to_string(),
                );
            }
            for child in object.values() {
                reject_external_refs(child)?;
            }
        }
        serde_json::Value::Array(array) => {
            for child in array {
                reject_external_refs(child)?;
            }
        }
        _ => {}
    }
    Ok(())
}

pub struct ValidateSchemaTool {
    max_schema_bytes: std::result::Result<usize, String>,
}

impl Default for ValidateSchemaTool {
    fn default() -> Self {
        Self::new()
    }
}

impl ValidateSchemaTool {
    pub fn new() -> Self {
        Self {
            max_schema_bytes: schema_limit_from_env(),
        }
    }

    #[cfg(test)]
    pub(crate) fn with_limit_for_test(max_schema_bytes: usize) -> Self {
        Self {
            max_schema_bytes: Ok(max_schema_bytes),
        }
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

    fn conversation_definition(&self) -> Result<serde_json::Value> {
        let max_schema_bytes = *self
            .max_schema_bytes
            .as_ref()
            .map_err(|message| IronCrewError::Validation(message.clone()))?;
        Ok(json!({
            "schema": self.schema(),
            "max_schema_bytes": max_schema_bytes,
        }))
    }

    async fn execute(&self, args: serde_json::Value, _ctx: &ToolCallContext) -> Result<String> {
        let data_str = args["data"]
            .as_str()
            .ok_or_else(|| IronCrewError::ToolExecution {
                tool: "validate_schema".into(),
                message: "Missing 'data' argument".into(),
            })?
            .to_owned();
        if data_str.len() > MAX_DATA_BYTES {
            return Err(IronCrewError::ToolExecution {
                tool: "validate_schema".into(),
                message: format!("JSON data exceeds {MAX_DATA_BYTES} bytes"),
            });
        }

        let schema_value = args
            .get("schema")
            .ok_or_else(|| IronCrewError::ToolExecution {
                tool: "validate_schema".into(),
                message: "Missing 'schema' argument".into(),
            })?
            .clone();
        let max_schema_bytes =
            *self
                .max_schema_bytes
                .as_ref()
                .map_err(|message| IronCrewError::ToolExecution {
                    tool: "validate_schema".into(),
                    message: message.clone(),
                })?;

        tokio::task::spawn_blocking(move || {
            let data: serde_json::Value =
                serde_json::from_str(&data_str).map_err(|e| IronCrewError::ToolExecution {
                    tool: "validate_schema".into(),
                    message: format!("Invalid JSON data: {e}"),
                })?;
            let validator = compile_local_draft7_with_limit(&schema_value, max_schema_bytes)
                .map_err(|e| IronCrewError::ToolExecution {
                    tool: "validate_schema".into(),
                    message: format!("Invalid JSON Schema: {e}"),
                })?;

            let mut errors = Vec::new();
            let mut truncated = false;
            for error in validator.iter_errors(&data) {
                if errors.len() == MAX_VALIDATION_ERRORS {
                    truncated = true;
                    break;
                }
                let path = error.instance_path().to_string();
                let message = error.to_string();
                errors.push(json!({
                    "path": crate::utils::http::utf8_prefix(&path, MAX_ERROR_FIELD_BYTES),
                    "message": crate::utils::http::utf8_prefix(&message, MAX_ERROR_FIELD_BYTES),
                }));
            }

            let result = json!({
                "valid": errors.is_empty(),
                "error_count": errors.len(),
                "errors_truncated": truncated,
                "errors": errors,
            });
            crate::utils::http::to_json_pretty_limited(&result, MAX_RESULT_BYTES).map_err(|e| {
                IronCrewError::ToolExecution {
                    tool: "validate_schema".into(),
                    message: format!("Validation result exceeds {MAX_RESULT_BYTES} bytes: {e}"),
                }
            })
        })
        .await
        .map_err(|e| IronCrewError::ToolExecution {
            tool: "validate_schema".into(),
            message: format!("Schema validation worker failed: {e}"),
        })?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_fragment_refs_remain_supported() {
        let schema = json!({
            "definitions": {
                "identifier": {"type": "integer"}
            },
            "type": "object",
            "properties": {
                "id": {"$ref": "#/definitions/identifier"}
            }
        });
        let validator = compile_local_draft7(&schema).expect("local ref must compile");
        assert!(validator.is_valid(&json!({"id": 7})));
        assert!(!validator.is_valid(&json!({"id": "seven"})));
    }

    #[tokio::test]
    async fn tool_rejects_remote_refs_before_validation() {
        let tool = ValidateSchemaTool::new();
        let error = tool
            .execute(
                json!({
                    "data": "{}",
                    "schema": {"$ref": "https://example.invalid/schema.json"}
                }),
                &ToolCallContext::default(),
            )
            .await
            .expect_err("remote ref must be rejected");
        assert!(error.to_string().contains("External JSON Schema $ref"));
    }
}
