//! MCP server configuration parsing and validation.
//!
//! # Security model
//! - Stdio commands are validated against `IRONCREW_MCP_ALLOWED_COMMANDS` allowlist (if set).
//! - HTTP URLs are validated via the existing SSRF filter; loopback is blocked unless
//!   `IRONCREW_MCP_ALLOW_LOCALHOST=1`.
//! - Server labels follow `^[a-z][a-z0-9_-]{0,15}$`.
//! - Final tool names (after `mcp__<server>__<tool>`) must be ≤ 64 characters and match
//!   `^[a-zA-Z0-9_-]{1,64}$`.

use std::collections::HashMap;

use regex::Regex;

use crate::utils::network::validate_url_not_private;

// ── regex helpers (compiled once) ───────────────────────────────────────────

fn server_label_regex() -> &'static Regex {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^[a-z][a-z0-9_-]{0,15}$").expect("valid regex"))
}

// ── public types ─────────────────────────────────────────────────────────────

/// How IronCrew should connect to a single MCP server.
#[derive(Debug, Clone)]
pub enum McpTransportConfig {
    /// Spawn a child process via stdio.
    Stdio {
        /// Binary / command to execute (e.g. `"uvx"`).
        command: String,
        /// Additional arguments.
        args: Vec<String>,
        /// Extra environment variables to pass to the child. Unrelated env is
        /// stripped unless `inherit_env = true` on [`McpServerConfig`].
        env: HashMap<String, String>,
    },
    /// Connect via HTTP Streamable transport.
    Http {
        /// Full URL including path, e.g. `"http://mcp.example.com/mcp"`.
        url: String,
        /// Optional extra HTTP headers (values are redacted in logs).
        headers: HashMap<String, String>,
    },
}

/// Per-server MCP configuration.
#[derive(Debug, Clone)]
pub struct McpServerConfig {
    /// User-supplied label used in tool-name prefix (`mcp__<label>__<tool>`).
    pub label: String,
    pub transport: McpTransportConfig,
    /// When true, the child process inherits the full parent environment.
    /// Defaults to `false` for security (keeps `OPENAI_API_KEY` etc. out of MCP children).
    pub inherit_env: bool,
}

/// Collection of MCP server configs for a crew.
#[derive(Debug, Clone, Default)]
pub struct McpConfig {
    pub servers: Vec<McpServerConfig>,
}

impl McpConfig {
    pub fn is_empty(&self) -> bool {
        self.servers.is_empty()
    }
}

// ── validation helpers ────────────────────────────────────────────────────────

/// Returns `Ok(())` if `label` matches `^[a-z][a-z0-9_-]{0,15}$`.
pub fn validate_server_label(label: &str) -> Result<(), String> {
    if server_label_regex().is_match(label) {
        Ok(())
    } else {
        Err(format!(
            "MCP server label '{}' is invalid. Must match ^[a-z][a-z0-9_-]{{0,15}}$",
            label
        ))
    }
}

/// Build the canonical `mcp__<server>__<tool>` name used in IronCrew's ToolRegistry.
/// Returns an error if the final name would exceed 64 characters.
pub fn make_tool_name(server_label: &str, raw_tool_name: &str) -> Result<String, String> {
    let composed = format!("mcp__{}__{}", server_label, raw_tool_name);
    if composed.len() > 64 {
        return Err(format!(
            "Composed MCP tool name '{}' exceeds 64 characters ({})",
            composed,
            composed.len()
        ));
    }
    Ok(composed)
}

/// Validate an MCP stdio command against the allowlist env var
/// `IRONCREW_MCP_ALLOWED_COMMANDS` (comma-separated binary names).
///
/// If the env var is unset, all commands are allowed (dev default).
pub fn validate_command_allowlist(command: &str) -> Result<(), String> {
    let allowlist_raw = match std::env::var("IRONCREW_MCP_ALLOWED_COMMANDS") {
        Ok(v) if !v.trim().is_empty() => v,
        _ => return Ok(()), // allowlist not set → allow all
    };

    let allowed: Vec<&str> = allowlist_raw.split(',').map(str::trim).collect();

    // Extract just the binary name (last path component) for comparison
    let binary_name = std::path::Path::new(command)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(command);

    if allowed.iter().any(|&a| a == binary_name || a == command) {
        Ok(())
    } else {
        Err(format!(
            "MCP stdio command '{}' is not in the allowed commands list. \
             Set IRONCREW_MCP_ALLOWED_COMMANDS to permit it.",
            command
        ))
    }
}

/// Validate an HTTP MCP URL.
///
/// Blocks private/loopback IPs via the SSRF filter unless
/// `IRONCREW_MCP_ALLOW_LOCALHOST=1` is set.
pub fn validate_mcp_http_url(url: &str) -> Result<(), String> {
    let allow_localhost = std::env::var("IRONCREW_MCP_ALLOW_LOCALHOST")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);

    // Temporarily mirror the check: if localhost is allowed, bypass SSRF filter entirely.
    // Otherwise run the full SSRF check (which blocks loopback).
    if allow_localhost {
        // Still parse the URL to ensure it's valid; skip IP-range check.
        url::Url::parse(url).map_err(|e| format!("Invalid MCP HTTP URL '{}': {}", url, e))?;
        Ok(())
    } else {
        validate_url_not_private(url).map_err(|e| {
            format!(
                "MCP HTTP URL '{}' failed SSRF validation: {}. \
                 Set IRONCREW_MCP_ALLOW_LOCALHOST=1 to allow localhost.",
                url, e
            )
        })
    }
}

// ── Lua table parser ──────────────────────────────────────────────────────────

/// Parse `mcp_servers` Lua table into a validated [`McpConfig`].
///
/// Expected shape:
/// ```lua
/// mcp_servers = {
///   git = {
///     transport = "stdio",
///     command   = "uvx",
///     args      = {"mcp-server-git"},
///     env       = { MY_VAR = "value" },
///     inherit_env = false,
///   },
///   myapi = {
///     transport = "http",
///     url       = "https://mcp.example.com/mcp",
///     headers   = { authorization = "Bearer TOKEN" },
///   },
/// }
/// ```
pub fn parse_mcp_config(table: &mlua::Table) -> Result<McpConfig, mlua::Error> {
    let mut servers = Vec::new();

    for pair in table.clone().pairs::<String, mlua::Table>() {
        let (label, server_table) = pair?;

        // Validate label
        validate_server_label(&label).map_err(mlua::Error::external)?;

        let transport_str: String = server_table
            .get::<String>("transport")
            .map_err(|_| mlua::Error::external("MCP server config missing 'transport' key"))?;

        let inherit_env: bool = server_table.get::<bool>("inherit_env").unwrap_or(false);

        let transport = match transport_str.as_str() {
            "stdio" => {
                let command: String = server_table
                    .get::<String>("command")
                    .map_err(|_| mlua::Error::external("MCP stdio config missing 'command' key"))?;

                validate_command_allowlist(&command).map_err(mlua::Error::external)?;

                let args: Vec<String> = server_table
                    .get::<mlua::Table>("args")
                    .map(|t| {
                        t.sequence_values::<String>()
                            .filter_map(|v| v.ok())
                            .collect()
                    })
                    .unwrap_or_default();

                let env: HashMap<String, String> = server_table
                    .get::<mlua::Table>("env")
                    .map(|t| t.pairs::<String, String>().filter_map(|p| p.ok()).collect())
                    .unwrap_or_default();

                McpTransportConfig::Stdio { command, args, env }
            }
            "http" => {
                let url: String = server_table
                    .get::<String>("url")
                    .map_err(|_| mlua::Error::external("MCP http config missing 'url' key"))?;

                validate_mcp_http_url(&url).map_err(mlua::Error::external)?;

                let headers: HashMap<String, String> = server_table
                    .get::<mlua::Table>("headers")
                    .map(|t| t.pairs::<String, String>().filter_map(|p| p.ok()).collect())
                    .unwrap_or_default();

                McpTransportConfig::Http { url, headers }
            }
            other => {
                return Err(mlua::Error::external(format!(
                    "MCP server '{}' has unknown transport '{}'. Expected 'stdio' or 'http'.",
                    label, other
                )));
            }
        };

        servers.push(McpServerConfig {
            label,
            transport,
            inherit_env,
        });
    }

    Ok(McpConfig { servers })
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── label validation ────────────────────────────────────────────────────

    #[test]
    fn valid_labels_accepted() {
        for label in &["git", "my-server", "s3_tool", "a1b2c3d4e5f6g7h8"] {
            assert!(
                validate_server_label(label).is_ok(),
                "Expected valid: {}",
                label
            );
        }
    }

    #[test]
    fn invalid_labels_rejected() {
        // Starts with digit
        assert!(validate_server_label("1bad").is_err());
        // Starts with hyphen
        assert!(validate_server_label("-bad").is_err());
        // Uppercase
        assert!(validate_server_label("Bad").is_err());
        // Too long (17 chars)
        assert!(validate_server_label("abcdefghijklmnopq").is_err());
        // Empty
        assert!(validate_server_label("").is_err());
    }

    // ── tool name composition ───────────────────────────────────────────────

    #[test]
    fn tool_name_too_long() {
        // mcp__s__ + 55 chars = 63 → ok boundary
        let long = "a".repeat(55);
        assert!(make_tool_name("s", &long).is_ok());
        // mcp__s__ + 56 chars = 64 → ok (exactly at limit)
        let longer = "a".repeat(56);
        let result = make_tool_name("s", &longer);
        assert!(result.is_ok()); // 8 + 56 = 64, exactly 64 is fine
        // mcp__s__ + 57 chars = 65 → too long
        let toolong = "a".repeat(57);
        assert!(make_tool_name("s", &toolong).is_err());
    }

    // ── allowlist enforcement ───────────────────────────────────────────────

    #[test]
    fn allowlist_blocks_unknown_command() {
        // SAFETY: single-threaded test; env mutation is safe here
        unsafe { std::env::set_var("IRONCREW_MCP_ALLOWED_COMMANDS", "uvx,npx") };
        let result = validate_command_allowlist("malicious-binary");
        // SAFETY: same
        unsafe { std::env::remove_var("IRONCREW_MCP_ALLOWED_COMMANDS") };
        assert!(result.is_err());
    }

    #[test]
    fn allowlist_permits_known_command() {
        // SAFETY: single-threaded test
        unsafe { std::env::set_var("IRONCREW_MCP_ALLOWED_COMMANDS", "uvx,npx") };
        let result = validate_command_allowlist("uvx");
        // SAFETY: same
        unsafe { std::env::remove_var("IRONCREW_MCP_ALLOWED_COMMANDS") };
        assert!(result.is_ok());
    }

    #[test]
    fn allowlist_unset_allows_all() {
        // SAFETY: single-threaded test
        unsafe { std::env::remove_var("IRONCREW_MCP_ALLOWED_COMMANDS") };
        assert!(validate_command_allowlist("anything").is_ok());
    }

    // ── URL SSRF blocking ───────────────────────────────────────────────────

    #[test]
    fn localhost_blocked_without_flag() {
        // SAFETY: single-threaded test
        unsafe { std::env::remove_var("IRONCREW_MCP_ALLOW_LOCALHOST") };
        assert!(validate_mcp_http_url("http://localhost:8000/mcp").is_err());
        assert!(validate_mcp_http_url("http://127.0.0.1:8000/mcp").is_err());
    }

    #[test]
    fn localhost_allowed_with_flag() {
        // SAFETY: single-threaded test
        unsafe { std::env::set_var("IRONCREW_MCP_ALLOW_LOCALHOST", "1") };
        let result = validate_mcp_http_url("http://localhost:8000/mcp");
        // SAFETY: same
        unsafe { std::env::remove_var("IRONCREW_MCP_ALLOW_LOCALHOST") };
        assert!(result.is_ok());
    }

    #[test]
    fn private_ip_blocked() {
        // SAFETY: single-threaded test
        unsafe {
            std::env::remove_var("IRONCREW_MCP_ALLOW_LOCALHOST");
            std::env::remove_var("IRONCREW_ALLOW_PRIVATE_IPS");
        }
        assert!(validate_mcp_http_url("http://192.168.1.10/mcp").is_err());
        assert!(validate_mcp_http_url("http://10.0.0.1/mcp").is_err());
    }

    #[test]
    fn public_url_passes() {
        // SAFETY: single-threaded test
        unsafe {
            std::env::remove_var("IRONCREW_MCP_ALLOW_LOCALHOST");
            std::env::remove_var("IRONCREW_ALLOW_PRIVATE_IPS");
        }
        // DNS resolution of a truly public host may not work in CI; test known-safe IP
        // We test the validation function with a raw public IP to avoid DNS flakiness.
        // 8.8.8.8 is Google's public DNS → not private/loopback/link-local.
        assert!(validate_mcp_http_url("http://8.8.8.8/mcp").is_ok());
    }
}
