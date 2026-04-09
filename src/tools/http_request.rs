use async_trait::async_trait;
use reqwest::Client;
use serde_json::json;
use std::collections::HashMap;
use std::sync::LazyLock;
use std::time::Duration;

use super::Tool;
use crate::llm::provider::ToolSchema;
use crate::utils::error::{IronCrewError, Result};

/// Shared HTTP client singleton — reused across all tool instances and Lua sandboxes.
/// Connection pool is shared, reducing memory and improving connection reuse.
pub static SHARED_HTTP_CLIENT: LazyLock<Client> = LazyLock::new(|| {
    Client::builder()
        .timeout(Duration::from_secs(30))
        .user_agent(format!("IronCrew/{}", env!("CARGO_PKG_VERSION")))
        .pool_max_idle_per_host(10)
        .build()
        .expect("Failed to build HTTP client")
});

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

    async fn execute(&self, args: serde_json::Value) -> Result<String> {
        let url = args["url"]
            .as_str()
            .ok_or_else(|| IronCrewError::ToolExecution {
                tool: "http_request".into(),
                message: "Missing 'url' argument".into(),
            })?;

        // SSRF protection: block private/internal IPs
        crate::utils::network::validate_url_not_private(url).map_err(|e| {
            IronCrewError::ToolExecution {
                tool: "http_request".into(),
                message: e,
            }
        })?;

        let method = args["method"].as_str().unwrap_or("GET").to_uppercase();

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
        if let Some(timeout) = args["timeout_secs"].as_f64() {
            req = req.timeout(Duration::from_secs_f64(timeout));
        }

        // Headers
        if let Some(headers) = args["headers"].as_object() {
            for (key, value) in headers {
                if let Some(v) = value.as_str() {
                    req = req.header(key.as_str(), v);
                }
            }
        }

        // Authentication
        if let Some(auth_type) = args["auth_type"].as_str() {
            match auth_type {
                "bearer" => {
                    if let Some(token) = args["auth_token"].as_str() {
                        req = req.header("Authorization", format!("Bearer {token}"));
                    }
                }
                "basic" => {
                    let username = args["auth_username"].as_str().unwrap_or("");
                    let password = args["auth_token"].as_str().unwrap_or("");
                    req = req.basic_auth(username, Some(password));
                }
                "api_key" => {
                    let header = args["auth_header"].as_str().unwrap_or("X-API-Key");
                    if let Some(key) = args["auth_token"].as_str() {
                        req = req.header(header, key);
                    }
                }
                _ => {}
            }
        }

        // Body
        if let Some(body) = args["body"].as_str() {
            if body.starts_with('{') || body.starts_with('[') {
                req = req
                    .header("Content-Type", "application/json")
                    .body(body.to_string());
            } else {
                req = req.body(body.to_string());
            }
        }

        // Send
        let resp = req.send().await.map_err(|e| IronCrewError::ToolExecution {
            tool: "http_request".into(),
            message: format!("Request failed: {e}"),
        })?;

        let status = resp.status().as_u16();
        let headers: HashMap<String, String> = resp
            .headers()
            .iter()
            .filter_map(|(k, v)| v.to_str().ok().map(|v| (k.to_string(), v.to_string())))
            .collect();

        // Enforce max response size BOTH via Content-Length header (cheap check)
        // AND via streaming read (covers chunked responses with no header).
        let max_response_size: usize = std::env::var("IRONCREW_MAX_RESPONSE_SIZE")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(50 * 1024 * 1024); // 50MB default

        if let Some(len) = resp.content_length()
            && len as usize > max_response_size
        {
            return Err(IronCrewError::ToolExecution {
                tool: "http_request".into(),
                message: format!(
                    "Response too large: {} bytes (limit: {} bytes)",
                    len, max_response_size
                ),
            });
        }

        // Stream the body, aborting as soon as the byte budget is exceeded.
        // This protects against responses that omit Content-Length.
        use futures::StreamExt;
        let mut stream = resp.bytes_stream();
        let mut body_bytes: Vec<u8> = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| IronCrewError::ToolExecution {
                tool: "http_request".into(),
                message: format!("Failed to read response: {e}"),
            })?;
            if body_bytes.len() + chunk.len() > max_response_size {
                return Err(IronCrewError::ToolExecution {
                    tool: "http_request".into(),
                    message: format!(
                        "Response exceeded max size of {} bytes while streaming",
                        max_response_size
                    ),
                });
            }
            body_bytes.extend_from_slice(&chunk);
        }
        let body_text = String::from_utf8_lossy(&body_bytes).into_owned();

        // Try to parse as JSON for pretty output
        let body_value: serde_json::Value =
            serde_json::from_str(&body_text).unwrap_or(serde_json::Value::String(body_text));

        let result = json!({
            "status": status,
            "headers": headers,
            "body": body_value,
        });

        serde_json::to_string_pretty(&result).map_err(|e| IronCrewError::ToolExecution {
            tool: "http_request".into(),
            message: format!("Failed to serialize response: {e}"),
        })
    }
}
