//! Environment scrubbing for the `shell` tool.
//!
//! Shell commands are model-controlled, and the process environment holds
//! provider API keys loaded from `.env`. An inherited environment would let any
//! executed command read those secrets back into the model's context, so the
//! child starts from a minimal allowlist instead. This mirrors the stdio-child
//! policy in `crate::mcp::client_connect`.

use std::collections::BTreeMap;

/// Variables a shell command legitimately needs to run at all.
const SAFE_ENV_KEYS: &[&str] = &["PATH", "HOME", "USER", "LANG", "TZ", "TERM"];

/// Operator-controlled additions, as a comma-separated list of exact names.
const PASSTHROUGH_ENV: &str = "IRONCREW_SHELL_ENV_PASSTHROUGH";

/// Build the child environment for a shell command from the current process
/// environment. Only the safe keys, `LC_*`, and operator-listed names survive.
pub(super) fn child_environment() -> BTreeMap<String, String> {
    let passthrough = std::env::var(PASSTHROUGH_ENV).unwrap_or_default();
    collect(std::env::vars(), &passthrough)
}

fn collect(
    variables: impl Iterator<Item = (String, String)>,
    passthrough: &str,
) -> BTreeMap<String, String> {
    variables
        .filter(|(key, _)| retains(key, passthrough))
        .collect()
}

fn retains(key: &str, passthrough: &str) -> bool {
    if SAFE_ENV_KEYS.contains(&key) || key.starts_with("LC_") {
        return true;
    }
    // Never let the passthrough list re-admit IronCrew's own configuration:
    // it carries store URLs, tokens, and HITL key material.
    if key.starts_with("IRONCREW_") {
        return false;
    }
    passthrough
        .split(',')
        .map(str::trim)
        .any(|allowed| !allowed.is_empty() && allowed == key)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env() -> Vec<(String, String)> {
        [
            ("PATH", "/usr/bin"),
            ("HOME", "/home/agent"),
            ("LC_ALL", "en_US.UTF-8"),
            ("OPENAI_API_KEY", "sk-secret"),
            ("ANTHROPIC_API_KEY", "sk-ant-secret"),
            ("AWS_SECRET_ACCESS_KEY", "aws-secret"),
            ("IRONCREW_API_TOKEN", "token"),
            ("BUILD_REGION", "eu-west"),
        ]
        .into_iter()
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect()
    }

    #[test]
    fn provider_keys_never_reach_the_child() {
        let child = collect(env().into_iter(), "");
        assert!(!child.contains_key("OPENAI_API_KEY"));
        assert!(!child.contains_key("ANTHROPIC_API_KEY"));
        assert!(!child.contains_key("AWS_SECRET_ACCESS_KEY"));
        assert!(!child.contains_key("IRONCREW_API_TOKEN"));
    }

    #[test]
    fn safe_keys_and_locale_survive() {
        let child = collect(env().into_iter(), "");
        assert_eq!(child.get("PATH").map(String::as_str), Some("/usr/bin"));
        assert_eq!(child.get("HOME").map(String::as_str), Some("/home/agent"));
        assert_eq!(child.get("LC_ALL").map(String::as_str), Some("en_US.UTF-8"));
    }

    #[test]
    fn operator_passthrough_admits_only_exact_names() {
        let child = collect(env().into_iter(), " BUILD_REGION , ");
        assert_eq!(
            child.get("BUILD_REGION").map(String::as_str),
            Some("eu-west")
        );

        let partial = collect(env().into_iter(), "BUILD");
        assert!(!partial.contains_key("BUILD_REGION"));
    }

    #[test]
    fn passthrough_cannot_re_admit_ironcrew_configuration() {
        let child = collect(env().into_iter(), "IRONCREW_API_TOKEN");
        assert!(!child.contains_key("IRONCREW_API_TOKEN"));
    }
}
