use async_trait::async_trait;
use reqwest::Client;
use serde_json::json;
use std::sync::LazyLock;
use std::time::Duration;

use super::{Tool, ToolCallContext};
use crate::llm::provider::ToolSchema;
use crate::utils::error::{IronCrewError, Result};

mod policy;
mod redirect_policy;
mod request;
pub(crate) use policy::HttpToolPolicy;
use redirect_policy::client_for_request;

/// Shared HTTP client singleton — reused across all tool instances and Lua sandboxes.
/// Connection pool is shared, reducing memory and improving connection reuse.
fn build_client(allow_private: bool) -> Client {
    crate::utils::network::secure_client_builder_with_private_access(
        crate::utils::network::OutboundNetworkPolicy::PublicOnly,
        allow_private,
    )
    .timeout(Duration::from_secs(30))
    .user_agent(format!("IronCrew/{}", env!("CARGO_PKG_VERSION")))
    .pool_max_idle_per_host(10)
    .build()
    .expect("Failed to build HTTP client")
}

pub(crate) static PUBLIC_HTTP_CLIENT: LazyLock<Client> = LazyLock::new(|| build_client(false));
pub(crate) static PRIVATE_HTTP_CLIENT: LazyLock<Client> = LazyLock::new(|| build_client(true));
pub static SHARED_HTTP_CLIENT: LazyLock<Client> = LazyLock::new(|| {
    if crate::utils::network::private_ips_override_enabled() {
        PRIVATE_HTTP_CLIENT.clone()
    } else {
        PUBLIC_HTTP_CLIENT.clone()
    }
});

pub(crate) fn client_for_policy(policy: &HttpToolPolicy) -> Client {
    if policy.allow_private() {
        PRIVATE_HTTP_CLIENT.clone()
    } else {
        PUBLIC_HTTP_CLIENT.clone()
    }
}

const MAX_REQUEST_TIMEOUT_SECS: f64 = 300.0;
const MAX_URL_BYTES: usize = 8 * 1024;
const MAX_REQUEST_HEADERS: usize = 128;
const DEFAULT_REQUEST_HEADER_BYTES: usize = 64 * 1024;
const HARD_REQUEST_HEADER_BYTES: usize = 1024 * 1024;
const DEFAULT_REQUEST_BODY_BYTES: usize = 8 * 1024 * 1024;
const HARD_REQUEST_BODY_BYTES: usize = 64 * 1024 * 1024;

fn request_argument_error(message: impl Into<String>) -> IronCrewError {
    IronCrewError::ToolExecution {
        tool: "http_request".into(),
        message: message.into(),
    }
}

pub struct HttpRequestTool {
    client: Client,
    policy: HttpToolPolicy,
}

impl Default for HttpRequestTool {
    fn default() -> Self {
        Self::new()
    }
}

impl HttpRequestTool {
    pub fn new() -> Self {
        let policy = HttpToolPolicy::capture();
        Self::with_policy(policy)
    }

    pub(crate) fn with_policy(policy: HttpToolPolicy) -> Self {
        let client = client_for_policy(&policy);
        Self { client, policy }
    }

    #[cfg(test)]
    pub(crate) fn with_policy_for_test(marker: usize, allow_private: bool) -> Self {
        let policy = HttpToolPolicy::from_values(marker, allow_private);
        let client = client_for_policy(&policy);
        Self { client, policy }
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

    fn conversation_definition(&self) -> Result<serde_json::Value> {
        Ok(json!({
            "schema": self.schema(),
            "policy": self.policy.definition(),
        }))
    }

    async fn execute(&self, args: serde_json::Value, _ctx: &ToolCallContext) -> Result<String> {
        let client = client_for_request(&self.policy, &args, &self.client);
        let req = request::build(&client, &args, &self.policy)?;

        // Send
        let resp = req.send().await.map_err(|e| IronCrewError::ToolExecution {
            tool: "http_request".into(),
            message: format!("Request failed: {e}"),
        })?;

        let status = resp.status().as_u16();
        let max_header_bytes = self.policy.response_header_bytes();
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
        let max_response_size = self.policy.response_bytes();
        let body_bytes =
            crate::utils::http::read_response_bytes(resp, max_response_size, "HTTP tool response")
                .await
                .map_err(|error| IronCrewError::ToolExecution {
                    tool: "http_request".into(),
                    message: error.to_string(),
                })?;

        // Try to parse as JSON for pretty output
        let max_json_bytes = self.policy.json_bytes();
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

        let max_output_bytes = self.policy.output_bytes();
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

#[cfg(test)]
mod redirect_credential_tests;
