use async_trait::async_trait;
use reqwest::Client;
use serde_json::json;
use std::sync::LazyLock;
use std::time::Duration;

use super::{Tool, ToolCallContext};
use crate::llm::provider::ToolSchema;
use crate::utils::error::{IronCrewError, Result};

/// Shared HTTP client singleton — reused across all tool instances and Lua sandboxes.
/// Connection pool is shared, reducing memory and improving connection reuse.
pub static SHARED_HTTP_CLIENT: LazyLock<Client> = LazyLock::new(|| {
    crate::utils::network::secure_client_builder(
        crate::utils::network::OutboundNetworkPolicy::PublicOnly,
    )
    .timeout(Duration::from_secs(30))
    .user_agent(format!("IronCrew/{}", env!("CARGO_PKG_VERSION")))
    .pool_max_idle_per_host(10)
    .build()
    .expect("Failed to build HTTP client")
});

const MAX_REQUEST_TIMEOUT_SECS: f64 = 300.0;
const MAX_URL_BYTES: usize = 8 * 1024;
const MAX_REQUEST_HEADERS: usize = 128;
const DEFAULT_REQUEST_HEADER_BYTES: usize = 64 * 1024;
const HARD_REQUEST_HEADER_BYTES: usize = 1024 * 1024;
const DEFAULT_REQUEST_BODY_BYTES: usize = 8 * 1024 * 1024;
const HARD_REQUEST_BODY_BYTES: usize = 64 * 1024 * 1024;

fn request_limit(name: &str, default: usize, hard_max: usize) -> usize {
    crate::utils::http::byte_limit_from_env(name, default).min(hard_max)
}

fn request_argument_error(message: impl Into<String>) -> IronCrewError {
    IronCrewError::ToolExecution {
        tool: "http_request".into(),
        message: message.into(),
    }
}

pub struct HttpRequestTool {
    client: Client,
}

impl Default for HttpRequestTool {
    fn default() -> Self {
        Self::new()
    }
}

impl HttpRequestTool {
    pub fn new() -> Self {
        Self {
            client: SHARED_HTTP_CLIENT.clone(),
        }
    }
}

#[async_trait]
impl Tool for HttpRequestTool {
    fn name(&self) -> &str {
        "http_request"
    }
    fn description(&self) -> &str {
        "Make an HTTP request (GET, POST, PUT, DELETE, PATCH) with optional headers, body, and authentication"
    }
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "http_request".into(),
            description: self.description().into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "url": { "type": "string", "description": "URL to request" },
                    "method": { "type": "string", "description": "HTTP method: GET, POST, PUT, DELETE, PATCH", "enum": ["GET", "POST", "PUT", "DELETE", "PATCH"] },
                    "headers": { "type": "object", "description": "Request headers as key-value pairs" },
                    "body": { "type": "string", "description": "Request body (for POST/PUT/PATCH)" },
                    "timeout_secs": { "type": "number", "description": "Request timeout in seconds (default 30)" },
                    "auth_type": { "type": "string", "description": "Authentication type: bearer, basic, api_key", "enum": ["bearer", "basic", "api_key"] },
                    "auth_token": { "type": "string", "description": "Auth token (for bearer), password (for basic), or key value (for api_key)" },
                    "auth_username": { "type": "string", "description": "Username for basic auth" },
                    "auth_header": { "type": "string", "description": "Header name for api_key auth (default: X-API-Key)" }
                },
                "required": ["url", "method"]
            }),
        }
    }

    async fn execute(&self, args: serde_json::Value, _ctx: &ToolCallContext) -> Result<String> {
        let url = args["url"]
            .as_str()
            .ok_or_else(|| request_argument_error("Missing 'url' argument"))?;
        if url.len() > MAX_URL_BYTES {
            return Err(request_argument_error(format!(
                "'url' exceeds the {MAX_URL_BYTES}-byte limit"
            )));
        }

        let method = args["method"]
            .as_str()
            .ok_or_else(|| request_argument_error("Missing or invalid 'method' argument"))?
            .to_uppercase();

        // Build request
        let mut req = match method.as_str() {
            "GET" => self.client.get(url),
            "POST" => self.client.post(url),
            "PUT" => self.client.put(url),
            "DELETE" => self.client.delete(url),
            "PATCH" => self.client.patch(url),
            other => {
                return Err(IronCrewError::ToolExecution {
                    tool: "http_request".into(),
                    message: format!("Unsupported method: {other}"),
                });
            }
        };

        // Timeout override
        if let Some(timeout_value) = args.get("timeout_secs")
            && !timeout_value.is_null()
        {
            let timeout = timeout_value
                .as_f64()
                .ok_or_else(|| IronCrewError::ToolExecution {
                    tool: "http_request".into(),
                    message: "'timeout_secs' must be a number".into(),
                })?;
            if !timeout.is_finite() || timeout <= 0.0 || timeout > MAX_REQUEST_TIMEOUT_SECS {
                return Err(IronCrewError::ToolExecution {
                    tool: "http_request".into(),
                    message: format!(
                        "'timeout_secs' must be finite and greater than 0, up to {MAX_REQUEST_TIMEOUT_SECS} seconds"
                    ),
                });
            }
            req = req.timeout(Duration::from_secs_f64(timeout));
        }

        // Headers
        let request_header_limit = request_limit(
            "IRONCREW_HTTP_MAX_REQUEST_HEADER_BYTES",
            DEFAULT_REQUEST_HEADER_BYTES,
            HARD_REQUEST_HEADER_BYTES,
        );
        let mut request_header_bytes = 0usize;
        let mut request_header_count = 0usize;
        if let Some(headers_value) = args.get("headers")
            && !headers_value.is_null()
        {
            let headers = headers_value
                .as_object()
                .ok_or_else(|| request_argument_error("'headers' must be an object"))?;
            if headers.len() > MAX_REQUEST_HEADERS {
                return Err(request_argument_error(format!(
                    "'headers' contains more than {MAX_REQUEST_HEADERS} entries"
                )));
            }
            for (key, value) in headers {
                let value = value.as_str().ok_or_else(|| {
                    request_argument_error(format!("header '{key}' value must be a string"))
                })?;
                request_header_bytes = request_header_bytes
                    .checked_add(key.len())
                    .and_then(|total| total.checked_add(value.len()))
                    .ok_or_else(|| request_argument_error("request headers are too large"))?;
                if request_header_bytes > request_header_limit {
                    return Err(request_argument_error(format!(
                        "request headers exceed IRONCREW_HTTP_MAX_REQUEST_HEADER_BYTES ({request_header_limit})"
                    )));
                }
                let name =
                    reqwest::header::HeaderName::from_bytes(key.as_bytes()).map_err(|_| {
                        request_argument_error(format!("invalid request header name '{key}'"))
                    })?;
                let value = reqwest::header::HeaderValue::from_str(value).map_err(|_| {
                    request_argument_error(format!("invalid value for request header '{key}'"))
                })?;
                req = req.header(name, value);
                request_header_count = request_header_count.saturating_add(1);
            }
        }

        // Authentication
        if let Some(auth_value) = args.get("auth_type")
            && !auth_value.is_null()
        {
            let auth_type = auth_value
                .as_str()
                .ok_or_else(|| request_argument_error("'auth_type' must be a string"))?;
            if request_header_count >= MAX_REQUEST_HEADERS {
                return Err(request_argument_error(format!(
                    "request would exceed the {MAX_REQUEST_HEADERS}-header limit"
                )));
            }
            match auth_type {
                "bearer" => {
                    let token = args["auth_token"]
                        .as_str()
                        .filter(|value| !value.is_empty())
                        .ok_or_else(|| {
                            request_argument_error(
                                "'auth_token' must be a non-empty string for bearer auth",
                            )
                        })?;
                    request_header_bytes = request_header_bytes
                        .checked_add("Authorization".len())
                        .and_then(|total| total.checked_add("Bearer ".len()))
                        .and_then(|total| total.checked_add(token.len()))
                        .ok_or_else(|| request_argument_error("request headers are too large"))?;
                    if request_header_bytes > request_header_limit {
                        return Err(request_argument_error(format!(
                            "request headers exceed IRONCREW_HTTP_MAX_REQUEST_HEADER_BYTES ({request_header_limit})"
                        )));
                    }
                    req = req.header("Authorization", format!("Bearer {token}"));
                }
                "basic" => {
                    let username = match args.get("auth_username") {
                        Some(value) if !value.is_null() => value.as_str().ok_or_else(|| {
                            request_argument_error("'auth_username' must be a string")
                        })?,
                        _ => "",
                    };
                    let password = match args.get("auth_token") {
                        Some(value) if !value.is_null() => value.as_str().ok_or_else(|| {
                            request_argument_error("'auth_token' must be a string")
                        })?,
                        _ => "",
                    };
                    let credential_bytes = username
                        .len()
                        .checked_add(password.len())
                        .and_then(|total| total.checked_add("AuthorizationBasic ".len()))
                        .ok_or_else(|| request_argument_error("request headers are too large"))?;
                    request_header_bytes = request_header_bytes
                        .checked_add(credential_bytes.saturating_mul(2))
                        .ok_or_else(|| request_argument_error("request headers are too large"))?;
                    if request_header_bytes > request_header_limit {
                        return Err(request_argument_error(format!(
                            "request headers exceed IRONCREW_HTTP_MAX_REQUEST_HEADER_BYTES ({request_header_limit})"
                        )));
                    }
                    req = req.basic_auth(username, Some(password));
                }
                "api_key" => {
                    let header = match args.get("auth_header") {
                        Some(value) if !value.is_null() => value.as_str().ok_or_else(|| {
                            request_argument_error("'auth_header' must be a string")
                        })?,
                        _ => "X-API-Key",
                    };
                    let key = args["auth_token"]
                        .as_str()
                        .filter(|value| !value.is_empty())
                        .ok_or_else(|| {
                            request_argument_error(
                                "'auth_token' must be a non-empty string for api_key auth",
                            )
                        })?;
                    request_header_bytes = request_header_bytes
                        .checked_add(header.len())
                        .and_then(|total| total.checked_add(key.len()))
                        .ok_or_else(|| request_argument_error("request headers are too large"))?;
                    if request_header_bytes > request_header_limit {
                        return Err(request_argument_error(format!(
                            "request headers exceed IRONCREW_HTTP_MAX_REQUEST_HEADER_BYTES ({request_header_limit})"
                        )));
                    }
                    let header = reqwest::header::HeaderName::from_bytes(header.as_bytes())
                        .map_err(|_| request_argument_error("invalid API-key header name"))?;
                    let key = reqwest::header::HeaderValue::from_str(key)
                        .map_err(|_| request_argument_error("invalid API-key header value"))?;
                    req = req.header(header, key);
                }
                other => {
                    return Err(request_argument_error(format!(
                        "unsupported auth_type '{other}'"
                    )));
                }
            }
        }

        // Body
        if let Some(body_value) = args.get("body")
            && !body_value.is_null()
        {
            let body = body_value
                .as_str()
                .ok_or_else(|| request_argument_error("'body' must be a string"))?;
            let body_limit = request_limit(
                "IRONCREW_HTTP_MAX_REQUEST_BODY_BYTES",
                DEFAULT_REQUEST_BODY_BYTES,
                HARD_REQUEST_BODY_BYTES,
            );
            if body.len() > body_limit {
                return Err(request_argument_error(format!(
                    "request body exceeds IRONCREW_HTTP_MAX_REQUEST_BODY_BYTES ({body_limit})"
                )));
            }
            if body.starts_with('{') || body.starts_with('[') {
                req = req
                    .header("Content-Type", "application/json")
                    .body(body.to_string());
            } else {
                req = req.body(body.to_string());
            }
        }

        // Eager SSRF validation gives a direct error; the client's protected
        // resolver repeats the policy for the address actually connected to.
        // Do it after cheap local argument validation so malformed tool calls
        // cannot trigger unnecessary DNS work.
        crate::utils::network::validate_url_not_private(url).map_err(|e| {
            IronCrewError::ToolExecution {
                tool: "http_request".into(),
                message: e,
            }
        })?;

        // Send
        let resp = req.send().await.map_err(|e| IronCrewError::ToolExecution {
            tool: "http_request".into(),
            message: format!("Request failed: {e}"),
        })?;

        let status = resp.status().as_u16();
        let max_header_bytes = crate::utils::http::byte_limit_from_env(
            "IRONCREW_HTTP_MAX_HEADER_BYTES",
            crate::utils::http::DEFAULT_HTTP_HEADER_BYTES,
        );
        let headers = crate::utils::http::collect_response_headers(
            resp.headers(),
            max_header_bytes,
            "HTTP tool response headers",
        )
        .map_err(|error| IronCrewError::ToolExecution {
            tool: "http_request".into(),
            message: error.to_string(),
        })?;

        // Enforce max response size BOTH via Content-Length header (cheap check)
        // AND via streaming read (covers chunked responses with no header).
        let max_response_size = crate::utils::http::byte_limit_from_env_with_legacy(
            "IRONCREW_HTTP_MAX_RESPONSE_BYTES",
            "IRONCREW_MAX_RESPONSE_SIZE",
            crate::utils::http::DEFAULT_HTTP_TOOL_RESPONSE_BYTES,
        );
        let body_bytes =
            crate::utils::http::read_response_bytes(resp, max_response_size, "HTTP tool response")
                .await
                .map_err(|error| IronCrewError::ToolExecution {
                    tool: "http_request".into(),
                    message: error.to_string(),
                })?;

        // Try to parse as JSON for pretty output
        let max_json_bytes = crate::utils::http::byte_limit_from_env(
            "IRONCREW_HTTP_MAX_JSON_BYTES",
            crate::utils::http::DEFAULT_HTTP_JSON_PARSE_BYTES,
        );
        let body_value: serde_json::Value = if body_bytes.len() <= max_json_bytes {
            match serde_json::from_slice(&body_bytes) {
                Ok(value) => value,
                Err(_) => {
                    serde_json::Value::String(String::from_utf8_lossy(&body_bytes).into_owned())
                }
            }
        } else {
            serde_json::Value::String(String::from_utf8_lossy(&body_bytes).into_owned())
        };
        drop(body_bytes);

        let result = json!({
            "status": status,
            "headers": headers,
            "body": body_value,
        });

        let max_output_bytes = crate::utils::http::byte_limit_from_env(
            "IRONCREW_HTTP_MAX_OUTPUT_BYTES",
            crate::utils::http::DEFAULT_HTTP_TOOL_OUTPUT_BYTES,
        );
        crate::utils::http::to_json_pretty_limited(&result, max_output_bytes).map_err(|e| {
            IronCrewError::ToolExecution {
                tool: "http_request".into(),
                message: format!("Failed to serialize response: {e}"),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn rejects_oversized_url_before_network_access() {
        let url = format!("https://example.com/{}", "x".repeat(MAX_URL_BYTES));
        let error = HttpRequestTool::new()
            .execute(
                serde_json::json!({"url": url, "method": "GET"}),
                &ToolCallContext::default(),
            )
            .await
            .expect_err("oversized URL must fail");
        assert!(error.to_string().contains("url"));
        assert!(error.to_string().contains("limit"));
    }

    #[tokio::test]
    async fn rejects_malformed_request_fields_strictly() {
        let cases = [
            serde_json::json!({"url": "https://example.com"}),
            serde_json::json!({"url": "https://example.com", "method": "GET", "headers": []}),
            serde_json::json!({"url": "https://example.com", "method": "POST", "body": 1}),
            serde_json::json!({"url": "https://example.com", "method": "GET", "auth_type": 1}),
            serde_json::json!({"url": "https://example.com", "method": "GET", "auth_type": "bearer"}),
        ];
        for args in cases {
            assert!(
                HttpRequestTool::new()
                    .execute(args, &ToolCallContext::default())
                    .await
                    .is_err()
            );
        }
    }

    #[tokio::test]
    async fn rejects_too_many_request_headers() {
        let mut headers = serde_json::Map::new();
        for index in 0..=MAX_REQUEST_HEADERS {
            headers.insert(format!("x-test-{index}"), serde_json::json!("value"));
        }
        let error = HttpRequestTool::new()
            .execute(
                serde_json::json!({
                    "url": "https://example.com",
                    "method": "GET",
                    "headers": headers,
                }),
                &ToolCallContext::default(),
            )
            .await
            .expect_err("header count must be bounded");
        assert!(error.to_string().contains("128"));
    }
}
