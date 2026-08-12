use std::time::Duration;

use serde_json::{Value, json};

use crate::utils::error::Result;

use super::shell_error;

const DEFAULT_TIMEOUT_SECS: u64 = 60;
pub(super) const MAX_TIMEOUT_SECS: u64 = 3_600;
const DEFAULT_MAX_OUTPUT_BYTES: usize = 1024 * 1024;
const HARD_MAX_OUTPUT_BYTES: usize = 16 * 1024 * 1024;

pub(super) struct ShellPolicy {
    default_timeout_secs: std::result::Result<u64, String>,
    max_output_bytes: std::result::Result<usize, String>,
}

impl ShellPolicy {
    pub(super) fn capture() -> Self {
        Self {
            default_timeout_secs: parse_env(
                "IRONCREW_SHELL_TIMEOUT_SECS",
                DEFAULT_TIMEOUT_SECS,
                MAX_TIMEOUT_SECS,
            ),
            max_output_bytes: parse_env(
                "IRONCREW_SHELL_MAX_OUTPUT_BYTES",
                DEFAULT_MAX_OUTPUT_BYTES,
                HARD_MAX_OUTPUT_BYTES,
            ),
        }
    }

    #[cfg(test)]
    pub(super) fn from_values(default_timeout_secs: u64, max_output_bytes: usize) -> Self {
        Self {
            default_timeout_secs: Ok(default_timeout_secs),
            max_output_bytes: Ok(max_output_bytes),
        }
    }

    pub(super) fn requested_timeout(&self, args: &Value) -> Result<Duration> {
        if let Some(value) = args.get("timeout_secs") {
            let seconds = value.as_u64().ok_or_else(|| {
                shell_error(format!(
                    "'timeout_secs' must be a positive integer no greater than {MAX_TIMEOUT_SECS}"
                ))
            })?;
            if !(1..=MAX_TIMEOUT_SECS).contains(&seconds) {
                return Err(shell_error(format!(
                    "'timeout_secs' must be from 1 to {MAX_TIMEOUT_SECS}"
                )));
            }
            return Ok(Duration::from_secs(seconds));
        }

        self.default_timeout_secs
            .as_ref()
            .copied()
            .map(Duration::from_secs)
            .map_err(|message| shell_error(message.clone()))
    }

    pub(super) fn max_output_bytes(&self) -> Result<usize> {
        self.max_output_bytes
            .as_ref()
            .copied()
            .map_err(|message| shell_error(message.clone()))
    }

    pub(super) fn definition(&self) -> Result<Value> {
        Ok(json!({
            "default_timeout_secs": self.default_timeout_secs.as_ref().copied()
                .map_err(|message| shell_error(message.clone()))?,
            "max_output_bytes": self.max_output_bytes()?,
        }))
    }
}

fn parse_env<T>(name: &'static str, default: T, max: T) -> std::result::Result<T, String>
where
    T: Copy + std::fmt::Display + std::str::FromStr + PartialOrd + From<u8>,
{
    let Some(raw) = std::env::var(name).ok() else {
        return Ok(default);
    };
    let value = raw
        .parse::<T>()
        .map_err(|_| format!("{name} must be an integer from 1 to {max}"))?;
    if value < T::from(1) || value > max {
        return Err(format!("{name} must be from 1 to {max}"));
    }
    Ok(value)
}
