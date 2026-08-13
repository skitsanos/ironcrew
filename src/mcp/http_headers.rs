//! User-configured MCP HTTP header validation and conversion.

use std::collections::HashMap;

use axum::http::{HeaderName, HeaderValue};

use crate::utils::error::{IronCrewError, Result};

pub(super) fn configured_header_map(
    headers: &HashMap<String, String>,
    server: &str,
) -> Result<HashMap<HeaderName, HeaderValue>> {
    headers
        .iter()
        .map(|(name, value)| {
            validate_configured_name(name).map_err(|message| IronCrewError::Mcp {
                server: server.to_owned(),
                message,
            })?;
            let parsed_name =
                HeaderName::from_bytes(name.as_bytes()).map_err(|error| IronCrewError::Mcp {
                    server: server.to_owned(),
                    message: format!("Invalid header name '{}': {error}", redacted(name)),
                })?;
            let parsed_value =
                HeaderValue::from_str(value).map_err(|error| IronCrewError::Mcp {
                    server: server.to_owned(),
                    message: format!("Invalid header value for '{}': {error}", redacted(name)),
                })?;
            Ok((parsed_name, parsed_value))
        })
        .collect()
}

fn validate_configured_name(name: &str) -> std::result::Result<(), String> {
    let lower = name.to_ascii_lowercase();
    let reserved = [
        "accept",
        "content-type",
        "content-length",
        "transfer-encoding",
        "host",
        "mcp-session-id",
        "last-event-id",
        "mcp-protocol-version",
        "mcp-method",
        "mcp-name",
    ];
    if reserved.contains(&lower.as_str()) || lower.starts_with("mcp-param-") {
        Err(format!(
            "Reserved MCP HTTP header '{}' cannot be configured",
            redacted(name)
        ))
    } else {
        Ok(())
    }
}

fn redacted(name: &str) -> &str {
    if matches!(
        name.to_ascii_lowercase().as_str(),
        "authorization"
            | "x-api-key"
            | "x-auth-token"
            | "cookie"
            | "proxy-authorization"
            | "set-cookie"
    ) {
        "[REDACTED]"
    } else {
        name
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_protocol_headers_but_allows_auth() {
        for name in ["MCP-Session-Id", "mCp-PrOtOcOl-VeRsIoN", "MCP-Param-X"] {
            assert!(validate_configured_name(name).is_err());
        }
        assert!(validate_configured_name("Authorization").is_ok());
        assert!(validate_configured_name("X-API-Key").is_ok());
    }
}
