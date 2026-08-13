//! Compiler for MCP 2026 `x-mcp-header` annotations.

use std::collections::HashSet;

use rmcp::model::Tool;
use serde_json::{Map, Value, json};

use super::http_tool_headers::{HTTP_HEADER_POLICY_VERSION, HeaderPolicyError};

#[derive(Clone, Debug)]
pub(super) struct HeaderPlan {
    rules: Vec<HeaderRule>,
}

#[derive(Clone, Debug)]
struct HeaderRule {
    path: Vec<String>,
    suffix: String,
    kind: PrimitiveKind,
}

#[derive(Clone, Copy, Debug)]
enum PrimitiveKind {
    String,
    Integer,
    Boolean,
}

pub(super) fn compile_tool(
    tool: &Tool,
) -> Result<(HeaderPlan, Map<String, Value>), HeaderPolicyError> {
    let mut rules = Vec::new();
    inspect_schema(
        &Value::Object(tool.input_schema.as_ref().clone()),
        &mut Vec::new(),
        true,
        &mut HashSet::new(),
        &mut rules,
    )?;
    rules.sort_by(|left, right| left.path.cmp(&right.path));
    let mut sanitized = Value::Object(tool.input_schema.as_ref().clone());
    strip_schema_annotations(&mut sanitized);
    Ok((
        HeaderPlan { rules },
        sanitized.as_object().cloned().unwrap_or_default(),
    ))
}

impl HeaderPlan {
    pub(super) fn headers(
        &self,
        arguments: Option<&Map<String, Value>>,
    ) -> Result<Vec<(axum::http::HeaderName, axum::http::HeaderValue)>, HeaderPolicyError> {
        let mut headers = Vec::with_capacity(self.rules.len());
        for rule in &self.rules {
            let Some(value) = arguments.and_then(|root| value_at(root, &rule.path)) else {
                continue;
            };
            if value.is_null() {
                continue;
            }
            let encoded = encode_header_value(&rule.kind.render(value, &rule.path.join("."))?);
            let header = format!("mcp-param-{}", rule.suffix);
            let name = axum::http::HeaderName::from_bytes(header.as_bytes())
                .map_err(|_| HeaderPolicyError::InvalidHeader(header.clone()))?;
            let value = axum::http::HeaderValue::from_str(&encoded)
                .map_err(|_| HeaderPolicyError::InvalidHeader(header))?;
            headers.push((name, value));
        }
        Ok(headers)
    }

    pub(super) fn definition(&self) -> Value {
        json!({
            "http_header_policy_version": HTTP_HEADER_POLICY_VERSION,
            "rules": self.rules.iter().map(HeaderRule::definition).collect::<Vec<_>>()
        })
    }
}

impl HeaderRule {
    fn definition(&self) -> Value {
        json!({"path": self.path, "header": self.suffix, "type": self.kind.name()})
    }
}

impl PrimitiveKind {
    fn parse(value: Option<&Value>, path: &str) -> Result<Self, HeaderPolicyError> {
        match value.and_then(Value::as_str) {
            Some("string") => Ok(Self::String),
            Some("integer") => Ok(Self::Integer),
            Some("boolean") => Ok(Self::Boolean),
            _ => Err(HeaderPolicyError::InvalidSchema(format!(
                "property `{path}` must have type string, integer, or boolean"
            ))),
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::String => "string",
            Self::Integer => "integer",
            Self::Boolean => "boolean",
        }
    }

    fn render(self, value: &Value, path: &str) -> Result<String, HeaderPolicyError> {
        match self {
            Self::String => value
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| HeaderPolicyError::InvalidArgument(path.to_owned())),
            Self::Boolean => value
                .as_bool()
                .map(|value| value.to_string())
                .ok_or_else(|| HeaderPolicyError::InvalidArgument(path.to_owned())),
            Self::Integer => safe_integer(value, path).map(|value| value.to_string()),
        }
    }
}

fn inspect_schema(
    value: &Value,
    path: &mut Vec<String>,
    reachable: bool,
    seen: &mut HashSet<String>,
    rules: &mut Vec<HeaderRule>,
) -> Result<(), HeaderPolicyError> {
    let Value::Object(schema) = value else {
        return Ok(());
    };
    if let Some(raw) = schema.get("x-mcp-header") {
        let location = path.join(".");
        if !reachable || path.is_empty() {
            return Err(HeaderPolicyError::InvalidSchema(format!(
                "annotation at `{location}` is not statically reachable through properties"
            )));
        }
        let suffix = raw
            .as_str()
            .filter(|value| is_token(value))
            .ok_or_else(|| {
                HeaderPolicyError::InvalidSchema(format!(
                    "property `{location}` has an invalid header name"
                ))
            })?;
        if !seen.insert(suffix.to_ascii_lowercase()) {
            return Err(HeaderPolicyError::InvalidSchema(format!(
                "duplicate header name `{suffix}`"
            )));
        }
        rules.push(HeaderRule {
            path: path.clone(),
            suffix: suffix.to_owned(),
            kind: PrimitiveKind::parse(schema.get("type"), &location)?,
        });
    }
    for (key, child) in schema {
        match schema_child_kind(key) {
            Some(SchemaChildren::Properties) => {
                if let Value::Object(properties) = child {
                    for (property, property_schema) in properties {
                        path.push(property.clone());
                        inspect_schema(property_schema, path, reachable, seen, rules)?;
                        path.pop();
                    }
                } else {
                    inspect_schema_values(child, path, seen, rules)?;
                }
            }
            Some(SchemaChildren::Map) => {
                if let Value::Object(schemas) = child {
                    for schema in schemas.values() {
                        inspect_schema(schema, path, false, seen, rules)?;
                    }
                } else {
                    inspect_schema_values(child, path, seen, rules)?;
                }
            }
            Some(SchemaChildren::Value) => {
                inspect_schema_values(child, path, seen, rules)?;
            }
            None => {}
        }
    }
    Ok(())
}

fn inspect_schema_values(
    value: &Value,
    path: &mut Vec<String>,
    seen: &mut HashSet<String>,
    rules: &mut Vec<HeaderRule>,
) -> Result<(), HeaderPolicyError> {
    match value {
        Value::Object(_) => inspect_schema(value, path, false, seen, rules),
        Value::Array(values) => values
            .iter()
            .try_for_each(|value| inspect_schema_values(value, path, seen, rules)),
        _ => Ok(()),
    }
}

fn strip_schema_annotations(value: &mut Value) {
    let Value::Object(schema) = value else {
        return;
    };
    schema.remove("x-mcp-header");
    for (key, child) in schema {
        match schema_child_kind(key) {
            Some(SchemaChildren::Properties | SchemaChildren::Map) => {
                if let Value::Object(schemas) = child {
                    schemas.values_mut().for_each(strip_schema_annotations);
                } else {
                    strip_schema_values(child);
                }
            }
            Some(SchemaChildren::Value) => strip_schema_values(child),
            None => {}
        }
    }
}

fn strip_schema_values(value: &mut Value) {
    match value {
        Value::Object(_) => strip_schema_annotations(value),
        Value::Array(values) => values.iter_mut().for_each(strip_schema_values),
        _ => {}
    }
}

#[derive(Clone, Copy)]
enum SchemaChildren {
    Properties,
    Map,
    Value,
}

fn schema_child_kind(keyword: &str) -> Option<SchemaChildren> {
    const MAP: &str = "$defs|definitions|patternProperties|dependentSchemas|dependencies";
    const VALUE: &str = "items|prefixItems|contains|additionalItems|additionalProperties|unevaluatedItems|unevaluatedProperties|propertyNames|allOf|anyOf|oneOf|not|if|then|else|contentSchema|extends";
    if keyword == "properties" {
        Some(SchemaChildren::Properties)
    } else if MAP.split('|').any(|candidate| candidate == keyword) {
        Some(SchemaChildren::Map)
    } else if VALUE.split('|').any(|candidate| candidate == keyword) {
        Some(SchemaChildren::Value)
    } else {
        None
    }
}

fn value_at<'a>(root: &'a Map<String, Value>, path: &[String]) -> Option<&'a Value> {
    let (first, rest) = path.split_first()?;
    rest.iter()
        .try_fold(root.get(first)?, |value, key| value.get(key))
}

fn safe_integer(value: &Value, path: &str) -> Result<i64, HeaderPolicyError> {
    const MAX_SAFE: i64 = 9_007_199_254_740_991;
    let invalid = || HeaderPolicyError::InvalidArgument(path.to_owned());
    let float = value.as_f64().ok_or_else(invalid)?;
    if !float.is_finite() || float.fract() != 0.0 {
        return Err(invalid());
    }
    if float.abs() > MAX_SAFE as f64 {
        return Err(HeaderPolicyError::UnsafeInteger(path.to_owned()));
    }
    Ok(float as i64)
}

fn is_token(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte))
}

fn encode_header_value(value: &str) -> String {
    use base64::{Engine as _, prelude::BASE64_STANDARD};
    let bytes = value.as_bytes();
    let sentinel = value.starts_with("=?base64?") && value.ends_with("?=");
    let unsafe_value = bytes
        .first()
        .is_some_and(|byte| matches!(byte, b' ' | b'\t'))
        || bytes
            .last()
            .is_some_and(|byte| matches!(byte, b' ' | b'\t'))
        || bytes
            .iter()
            .any(|byte| !matches!(byte, b'\t' | 0x20..=0x7e));
    if unsafe_value || sentinel {
        format!("=?base64?{}?=", BASE64_STANDARD.encode(bytes))
    } else {
        value.to_owned()
    }
}
