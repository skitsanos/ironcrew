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

use crate::utils::network::{OutboundNetworkPolicy, validate_url_with_policy};

const MAX_MCP_SERVERS: usize = 16;
const MAX_COMMAND_BYTES: usize = 1024;
const MAX_ARGUMENTS: usize = 128;
const MAX_ARGUMENT_BYTES: usize = 4096;
const MAX_ARGUMENTS_TOTAL_BYTES: usize = 64 * 1024;
const MAX_ENV_ENTRIES: usize = 64;
const MAX_ENV_KEY_BYTES: usize = 128;
const MAX_ENV_VALUE_BYTES: usize = 16 * 1024;
const MAX_ENV_TOTAL_BYTES: usize = 256 * 1024;
const MAX_HEADERS: usize = 64;
const MAX_HEADER_NAME_BYTES: usize = 128;
const MAX_HEADER_VALUE_BYTES: usize = 16 * 1024;
const MAX_HEADERS_TOTAL_BYTES: usize = 256 * 1024;
const MAX_MCP_URL_BYTES: usize = 2048;

fn validate_bounded_text(
    field: &str,
    value: &str,
    max_bytes: usize,
    allow_control: bool,
) -> Result<(), mlua::Error> {
    if value.is_empty() || value.len() > max_bytes {
        return Err(mlua::Error::external(format!(
            "{field} must contain 1..={max_bytes} bytes"
        )));
    }
    if !allow_control && value.chars().any(char::is_control) {
        return Err(mlua::Error::external(format!(
            "{field} must not contain control characters"
        )));
    }
    Ok(())
}

fn parse_dense_string_list(
    table: mlua::Table,
    field: &str,
    max_entries: usize,
    max_item_bytes: usize,
    max_total_bytes: usize,
) -> Result<Vec<String>, mlua::Error> {
    let len = table.raw_len();
    if len > max_entries {
        return Err(mlua::Error::external(format!(
            "{field} has {len} entries; limit is {max_entries}"
        )));
    }
    let mut seen = 0usize;
    for pair in table.clone().pairs::<mlua::Value, mlua::Value>() {
        let (key, _) = pair?;
        seen = seen.saturating_add(1);
        if !matches!(key, mlua::Value::Integer(index) if index >= 1 && (index as usize) <= len) {
            return Err(mlua::Error::external(format!(
                "{field} must be a dense array with integer keys starting at 1"
            )));
        }
    }
    if seen != len {
        return Err(mlua::Error::external(format!(
            "{field} must not contain gaps"
        )));
    }

    let mut values = Vec::with_capacity(len);
    let mut total = 0usize;
    for index in 1..=len {
        let value = table
            .raw_get::<String>(index)
            .map_err(|_| mlua::Error::external(format!("{field}[{index}] must be a string")))?;
        validate_bounded_text(field, &value, max_item_bytes, false)?;
        total = total
            .checked_add(value.len())
            .ok_or_else(|| mlua::Error::external(format!("{field} size overflowed")))?;
        if total > max_total_bytes {
            return Err(mlua::Error::external(format!(
                "{field} uses {total} bytes; limit is {max_total_bytes}"
            )));
        }
        values.push(value);
    }
    Ok(values)
}

#[allow(clippy::too_many_arguments)]
fn parse_string_map(
    table: mlua::Table,
    field: &str,
    max_entries: usize,
    max_key_bytes: usize,
    max_value_bytes: usize,
    max_total_bytes: usize,
    allow_value_control: bool,
) -> Result<HashMap<String, String>, mlua::Error> {
    let mut values = HashMap::new();
    let mut total = 0usize;
    for pair in table.pairs::<mlua::Value, mlua::Value>() {
        let (key, value) = pair?;
        if values.len() >= max_entries {
            return Err(mlua::Error::external(format!(
                "{field} exceeds the {max_entries}-entry limit"
            )));
        }
        let mlua::Value::String(key) = key else {
            return Err(mlua::Error::external(format!(
                "{field} keys must be strings"
            )));
        };
        let mlua::Value::String(value) = value else {
            return Err(mlua::Error::external(format!(
                "{field} values must be strings"
            )));
        };
        let key = key.to_str()?.to_owned();
        let value = value.to_str()?.to_owned();
        validate_bounded_text(&format!("{field} key"), &key, max_key_bytes, false)?;
        if value.len() > max_value_bytes
            || (!allow_value_control && value.chars().any(char::is_control))
        {
            return Err(mlua::Error::external(format!(
                "{field} value must be at most {max_value_bytes} bytes"
            )));
        }
        total = total
            .checked_add(key.len())
            .and_then(|size| size.checked_add(value.len()))
            .ok_or_else(|| mlua::Error::external(format!("{field} size overflowed")))?;
        if total > max_total_bytes {
            return Err(mlua::Error::external(format!(
                "{field} uses {total} bytes; limit is {max_total_bytes}"
            )));
        }
        values.insert(key, value);
    }
    Ok(values)
}

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
    /// Digest of an explicit non-secret server version/deployment identity.
    /// Persistent conversations fail closed when a selected MCP tool's
    /// server does not provide one.
    pub execution_identity_fingerprint: Option<String>,
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
    if !composed
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return Err(format!(
            "Composed MCP tool name '{}' contains unsupported characters",
            composed
        ));
    }
    Ok(composed)
}

/// Validate an MCP stdio command against the allowlist env var
/// `IRONCREW_MCP_ALLOWED_COMMANDS` (comma-separated commands).
///
/// If the env var is unset, all commands are allowed (dev default).
pub fn validate_command_allowlist(command: &str) -> Result<(), String> {
    let allowlist_raw = match std::env::var("IRONCREW_MCP_ALLOWED_COMMANDS") {
        Ok(value) if !value.trim().is_empty() => value,
        Ok(_) => {
            return Err(
                "IRONCREW_MCP_ALLOWED_COMMANDS is present but empty; refusing all MCP stdio commands"
                    .into(),
            );
        }
        Err(std::env::VarError::NotPresent) => return Ok(()),
        Err(std::env::VarError::NotUnicode(_)) => {
            return Err("IRONCREW_MCP_ALLOWED_COMMANDS must contain valid UTF-8".into());
        }
    };

    if allowlist_raw.trim() == "__disabled__" {
        return Err("MCP stdio commands are disabled by IRONCREW_MCP_ALLOWED_COMMANDS".into());
    }

    // Exact matching is deliberate. In particular, an allowlisted basename
    // such as `npx` must not authorize `/tmp/npx` or `./npx`.
    if allowlist_raw
        .split(',')
        .map(str::trim)
        .any(|allowed| !allowed.is_empty() && allowed == command)
    {
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

    let policy = if allow_localhost {
        OutboundNetworkPolicy::AllowLoopback
    } else {
        OutboundNetworkPolicy::PublicOnly
    };
    validate_url_with_policy(url, policy).map_err(|e| {
        format!(
            "MCP HTTP URL '{}' failed SSRF validation: {}. \
             Set IRONCREW_MCP_ALLOW_LOCALHOST=1 to allow loopback only.",
            url, e
        )
    })?;

    let allowlist = match std::env::var("IRONCREW_MCP_ALLOWED_HTTP_HOSTS") {
        Ok(value) if !value.trim().is_empty() => value,
        Ok(_) => {
            return Err(
                "IRONCREW_MCP_ALLOWED_HTTP_HOSTS is present but empty; refusing all HTTP MCP servers"
                    .into(),
            );
        }
        Err(std::env::VarError::NotPresent) => return Ok(()),
        Err(std::env::VarError::NotUnicode(_)) => {
            return Err("IRONCREW_MCP_ALLOWED_HTTP_HOSTS must contain valid UTF-8".into());
        }
    };
    if allowlist.trim() == "__disabled__" {
        return Err("HTTP MCP servers are disabled by IRONCREW_MCP_ALLOWED_HTTP_HOSTS".into());
    }
    let parsed = url::Url::parse(url).map_err(|error| format!("Invalid MCP HTTP URL: {error}"))?;
    let host = parsed
        .host_str()
        .ok_or_else(|| "MCP HTTP URL must have a host".to_string())?;
    if allowlist
        .split(',')
        .map(str::trim)
        .any(|allowed| !allowed.is_empty() && allowed.eq_ignore_ascii_case(host))
    {
        Ok(())
    } else {
        Err(format!(
            "MCP HTTP host '{host}' is not in IRONCREW_MCP_ALLOWED_HTTP_HOSTS"
        ))
    }
}

// ── Lua table parser ──────────────────────────────────────────────────────────

/// Parse `mcp_servers` Lua table into a validated [`McpConfig`].
///
/// Expected shape:
/// ```lua
/// mcp_servers = {
///   local = {
///     transport = "stdio",
///     execution_identity = "local-mcp-2026-07-28-v1",
///     command   = "python3",
///     args      = {"examples/mcp/stdio-tools/server.py"},
///     env       = { MY_VAR = "value" },
///     inherit_env = false,
///   },
///   myapi = {
///     transport = "http",
///     execution_identity = "catalog-api-v3",
///     url       = "https://mcp.example.com/mcp",
///     headers   = { authorization = "Bearer TOKEN" },
///   },
/// }
/// ```
pub fn parse_mcp_config(table: &mlua::Table) -> Result<McpConfig, mlua::Error> {
    let mut servers = Vec::new();

    for pair in table.clone().pairs::<String, mlua::Table>() {
        let (label, server_table) = pair?;

        if servers.len() >= MAX_MCP_SERVERS {
            return Err(mlua::Error::external(format!(
                "mcp_servers exceeds the {MAX_MCP_SERVERS}-server limit"
            )));
        }

        // Validate label
        validate_server_label(&label).map_err(mlua::Error::external)?;

        let transport_str: String = server_table
            .get::<String>("transport")
            .map_err(|_| mlua::Error::external("MCP server config missing 'transport' key"))?;

        let inherit_env = server_table
            .get::<Option<bool>>("inherit_env")?
            .unwrap_or(false);
        let execution_identity_fingerprint = server_table
            .get::<Option<String>>("execution_identity")?
            .map(|identity| {
                crate::engine::conversation_provider::explicit_execution_identity_fingerprint(
                    "mcp-server",
                    &label,
                    &identity,
                )
                .map_err(mlua::Error::external)
            })
            .transpose()?;

        let transport = match transport_str.as_str() {
            "stdio" => {
                let command: String = server_table
                    .get::<String>("command")
                    .map_err(|_| mlua::Error::external("MCP stdio config missing 'command' key"))?;

                validate_bounded_text("MCP command", &command, MAX_COMMAND_BYTES, false)?;

                validate_command_allowlist(&command).map_err(mlua::Error::external)?;

                let args: Vec<String> = match server_table.get::<Option<mlua::Table>>("args")? {
                    Some(args) => parse_dense_string_list(
                        args,
                        "MCP args",
                        MAX_ARGUMENTS,
                        MAX_ARGUMENT_BYTES,
                        MAX_ARGUMENTS_TOTAL_BYTES,
                    )?,
                    None => Vec::new(),
                };

                let env: HashMap<String, String> =
                    match server_table.get::<Option<mlua::Table>>("env")? {
                        Some(env) => parse_string_map(
                            env,
                            "MCP env",
                            MAX_ENV_ENTRIES,
                            MAX_ENV_KEY_BYTES,
                            MAX_ENV_VALUE_BYTES,
                            MAX_ENV_TOTAL_BYTES,
                            true,
                        )?,
                        None => HashMap::new(),
                    };

                McpTransportConfig::Stdio { command, args, env }
            }
            "http" => {
                let url: String = server_table
                    .get::<String>("url")
                    .map_err(|_| mlua::Error::external("MCP http config missing 'url' key"))?;

                validate_bounded_text("MCP HTTP URL", &url, MAX_MCP_URL_BYTES, false)?;

                validate_mcp_http_url(&url).map_err(mlua::Error::external)?;

                let headers: HashMap<String, String> =
                    match server_table.get::<Option<mlua::Table>>("headers")? {
                        Some(headers) => parse_string_map(
                            headers,
                            "MCP headers",
                            MAX_HEADERS,
                            MAX_HEADER_NAME_BYTES,
                            MAX_HEADER_VALUE_BYTES,
                            MAX_HEADERS_TOTAL_BYTES,
                            false,
                        )?,
                        None => HashMap::new(),
                    };

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
            execution_identity_fingerprint,
            inherit_env,
        });
    }

    Ok(McpConfig { servers })
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Serialize env-mutating tests. `cargo test` runs tests in parallel
    /// by default, and the tests below flip process-wide env vars that
    /// `validate_*` reads — without a shared lock they race. Every test
    /// that touches `std::env::set_var` / `remove_var` must hold this
    /// guard for its whole duration. Using `std::sync::Mutex` (not
    /// `parking_lot`) avoids a new dependency.
    fn env_guard() -> std::sync::MutexGuard<'static, ()> {
        use std::sync::{Mutex, OnceLock};
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        // Poison recovery: a prior test that panicked while holding the
        // lock would poison it. We don't care — the state we care about
        // is reset by the next test's own cleanup.
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

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

    #[test]
    fn explicit_execution_identity_is_secret_safe_and_versioned() {
        let _guard = env_guard();
        unsafe { std::env::remove_var("IRONCREW_MCP_ALLOWED_COMMANDS") };
        let parse = |identity: Option<&str>, secret: &str| {
            let lua = mlua::Lua::new();
            let identity = identity
                .map(|value| format!("execution_identity = '{value}',"))
                .unwrap_or_default();
            let table = lua
                .load(format!(
                    "return {{ svc = {{ transport = 'stdio', command = 'mcp-test', {identity} env = {{ TOKEN = '{secret}' }} }} }}"
                ))
                .eval::<mlua::Table>()
                .unwrap();
            parse_mcp_config(&table).unwrap().servers.remove(0)
        };

        let first = parse(Some("catalog-v1"), "secret-one");
        let rotated = parse(Some("catalog-v1"), "secret-two");
        let upgraded = parse(Some("catalog-v2"), "secret-one");
        assert_eq!(
            first.execution_identity_fingerprint,
            rotated.execution_identity_fingerprint
        );
        assert_ne!(
            first.execution_identity_fingerprint,
            upgraded.execution_identity_fingerprint
        );
        assert!(
            parse(None, "secret-one")
                .execution_identity_fingerprint
                .is_none()
        );
        assert!(
            first
                .execution_identity_fingerprint
                .as_deref()
                .is_some_and(|fingerprint| !fingerprint.contains("secret-one"))
        );
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

    #[test]
    fn tool_name_rejects_protocol_unsafe_characters() {
        assert!(make_tool_name("git", "status.ok").is_err());
        assert!(make_tool_name("git", "status/../../exec").is_err());
    }

    // ── allowlist enforcement ───────────────────────────────────────────────

    #[test]
    fn allowlist_blocks_unknown_command() {
        let _guard = env_guard();
        unsafe { std::env::set_var("IRONCREW_MCP_ALLOWED_COMMANDS", "uvx,npx") };
        let result = validate_command_allowlist("malicious-binary");
        unsafe { std::env::remove_var("IRONCREW_MCP_ALLOWED_COMMANDS") };
        assert!(result.is_err());
    }

    #[test]
    fn allowlist_permits_known_command() {
        let _guard = env_guard();
        unsafe { std::env::set_var("IRONCREW_MCP_ALLOWED_COMMANDS", "uvx,npx") };
        let result = validate_command_allowlist("uvx");
        unsafe { std::env::remove_var("IRONCREW_MCP_ALLOWED_COMMANDS") };
        assert!(result.is_ok());
    }

    #[test]
    fn basename_allowlist_does_not_authorize_path_qualified_command() {
        let _guard = env_guard();
        unsafe { std::env::set_var("IRONCREW_MCP_ALLOWED_COMMANDS", "npx") };
        assert!(validate_command_allowlist("/tmp/npx").is_err());
        assert!(validate_command_allowlist("./npx").is_err());
        unsafe { std::env::remove_var("IRONCREW_MCP_ALLOWED_COMMANDS") };
    }

    #[test]
    fn exact_path_can_be_allowlisted_explicitly() {
        let _guard = env_guard();
        unsafe { std::env::set_var("IRONCREW_MCP_ALLOWED_COMMANDS", "/usr/bin/npx") };
        assert!(validate_command_allowlist("/usr/bin/npx").is_ok());
        assert!(validate_command_allowlist("/tmp/npx").is_err());
        unsafe { std::env::remove_var("IRONCREW_MCP_ALLOWED_COMMANDS") };
    }

    #[test]
    fn allowlist_unset_allows_all() {
        let _guard = env_guard();
        unsafe { std::env::remove_var("IRONCREW_MCP_ALLOWED_COMMANDS") };
        assert!(validate_command_allowlist("anything").is_ok());
    }

    #[test]
    fn allowlist_present_but_empty_fails_closed() {
        let _guard = env_guard();
        unsafe { std::env::set_var("IRONCREW_MCP_ALLOWED_COMMANDS", "  ") };
        assert!(validate_command_allowlist("anything").is_err());
        unsafe { std::env::remove_var("IRONCREW_MCP_ALLOWED_COMMANDS") };
    }

    #[test]
    fn disabled_sentinel_refuses_every_stdio_command() {
        let _guard = env_guard();
        unsafe { std::env::set_var("IRONCREW_MCP_ALLOWED_COMMANDS", "__disabled__") };
        let result = validate_command_allowlist("__disabled__");
        unsafe { std::env::remove_var("IRONCREW_MCP_ALLOWED_COMMANDS") };
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("disabled"));
    }

    // ── URL SSRF blocking ───────────────────────────────────────────────────

    #[test]
    fn localhost_blocked_without_flag() {
        let _guard = env_guard();
        unsafe {
            std::env::remove_var("IRONCREW_MCP_ALLOW_LOCALHOST");
            std::env::remove_var("IRONCREW_MCP_ALLOWED_HTTP_HOSTS");
        };
        assert!(validate_mcp_http_url("http://localhost:8000/mcp").is_err());
        assert!(validate_mcp_http_url("http://127.0.0.1:8000/mcp").is_err());
    }

    #[test]
    fn localhost_allowed_with_flag() {
        let _guard = env_guard();
        unsafe {
            std::env::set_var("IRONCREW_MCP_ALLOW_LOCALHOST", "1");
            std::env::remove_var("IRONCREW_MCP_ALLOWED_HTTP_HOSTS");
        };
        let result = validate_mcp_http_url("http://localhost:8000/mcp");
        unsafe { std::env::remove_var("IRONCREW_MCP_ALLOW_LOCALHOST") };
        assert!(result.is_ok());
    }

    #[test]
    fn localhost_flag_does_not_allow_other_private_ranges() {
        let _guard = env_guard();
        unsafe {
            std::env::set_var("IRONCREW_MCP_ALLOW_LOCALHOST", "1");
            std::env::remove_var("IRONCREW_ALLOW_PRIVATE_IPS");
            std::env::remove_var("IRONCREW_MCP_ALLOWED_HTTP_HOSTS");
        }
        assert!(validate_mcp_http_url("http://10.0.0.1/mcp").is_err());
        assert!(validate_mcp_http_url("http://192.168.1.2/mcp").is_err());
        assert!(validate_mcp_http_url("http://169.254.169.254/mcp").is_err());
        unsafe { std::env::remove_var("IRONCREW_MCP_ALLOW_LOCALHOST") };
    }

    #[test]
    fn private_ip_blocked() {
        let _guard = env_guard();
        unsafe {
            std::env::remove_var("IRONCREW_MCP_ALLOW_LOCALHOST");
            std::env::remove_var("IRONCREW_ALLOW_PRIVATE_IPS");
            std::env::remove_var("IRONCREW_MCP_ALLOWED_HTTP_HOSTS");
        }
        assert!(validate_mcp_http_url("http://192.168.1.10/mcp").is_err());
        assert!(validate_mcp_http_url("http://10.0.0.1/mcp").is_err());
    }

    #[test]
    fn public_url_passes() {
        let _guard = env_guard();
        unsafe {
            std::env::remove_var("IRONCREW_MCP_ALLOW_LOCALHOST");
            std::env::remove_var("IRONCREW_ALLOW_PRIVATE_IPS");
            std::env::remove_var("IRONCREW_MCP_ALLOWED_HTTP_HOSTS");
        }
        // DNS resolution of a truly public host may not work in CI; test known-safe IP
        // We test the validation function with a raw public IP to avoid DNS flakiness.
        // 8.8.8.8 is Google's public DNS → not private/loopback/link-local.
        assert!(validate_mcp_http_url("http://8.8.8.8/mcp").is_ok());
    }

    #[test]
    fn http_host_allowlist_is_exact_and_supports_disable_sentinel() {
        let _guard = env_guard();
        unsafe { std::env::set_var("IRONCREW_MCP_ALLOWED_HTTP_HOSTS", "8.8.8.8") };
        assert!(validate_mcp_http_url("https://8.8.8.8/mcp").is_ok());
        assert!(validate_mcp_http_url("https://8.8.4.4/mcp").is_err());
        unsafe { std::env::set_var("IRONCREW_MCP_ALLOWED_HTTP_HOSTS", "__disabled__") };
        assert!(validate_mcp_http_url("https://8.8.8.8/mcp").is_err());
        unsafe { std::env::remove_var("IRONCREW_MCP_ALLOWED_HTTP_HOSTS") };
    }
}
