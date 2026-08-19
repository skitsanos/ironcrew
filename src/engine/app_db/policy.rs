//! Fail-closed limits for the app-data capability, captured once.

use std::time::Duration;

use crate::engine::pg_runtime::PgConnectSettings;
use crate::utils::error::{IronCrewError, Result};

const DEFAULTS: [(&str, u64, u64); 7] = [
    ("IRONCREW_APP_DB_MAX_CONNECTIONS", 4, 32),
    ("IRONCREW_APP_DB_STATEMENT_TIMEOUT_MS", 5_000, 60_000),
    ("IRONCREW_APP_DB_MAX_ROWS", 500, 10_000),
    (
        "IRONCREW_APP_DB_MAX_RESPONSE_BYTES",
        1024 * 1024,
        16 * 1024 * 1024,
    ),
    (
        "IRONCREW_APP_DB_MAX_PARAM_BYTES",
        1024 * 1024,
        16 * 1024 * 1024,
    ),
    ("IRONCREW_APP_DB_MAX_OPERATIONS", 64, 256),
    ("IRONCREW_APP_DB_MAX_SQL_BYTES", 64 * 1024, 1024 * 1024),
];

#[derive(Debug, Clone)]
pub struct AppDbPolicy {
    values: [u64; 7],
}

fn env_limit(name: &str, default: u64, ceiling: u64) -> Result<u64> {
    match std::env::var(name) {
        Err(std::env::VarError::NotPresent) => Ok(default),
        Err(std::env::VarError::NotUnicode(_)) => Err(IronCrewError::Validation(format!(
            "{name} must contain valid UTF-8 and be an integer between 1 and {ceiling}"
        ))),
        Ok(raw) => {
            let value = raw.parse::<u64>().map_err(|_| {
                IronCrewError::Validation(format!(
                    "{name} must be an integer between 1 and {ceiling}"
                ))
            })?;
            if value == 0 || value > ceiling {
                return Err(IronCrewError::Validation(format!(
                    "{name} must be between 1 and {ceiling}; got {value}"
                )));
            }
            Ok(value)
        }
    }
}

impl AppDbPolicy {
    pub fn capture() -> Result<Self> {
        let mut values = [0u64; 7];
        for (index, (name, default, ceiling)) in DEFAULTS.iter().enumerate() {
            values[index] = env_limit(name, *default, *ceiling)?;
        }
        Ok(Self { values })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn from_values(
        max_connections: u64,
        statement_timeout_ms: u64,
        max_rows: u64,
        max_response_bytes: u64,
        max_param_bytes: u64,
        max_operations: u64,
        max_sql_bytes: u64,
    ) -> Self {
        Self {
            values: [
                max_connections,
                statement_timeout_ms,
                max_rows,
                max_response_bytes,
                max_param_bytes,
                max_operations,
                max_sql_bytes,
            ],
        }
    }

    pub fn max_connections(&self) -> u32 {
        self.values[0] as u32
    }
    pub fn statement_timeout_ms(&self) -> u64 {
        self.values[1]
    }
    pub fn max_rows(&self) -> usize {
        self.values[2] as usize
    }
    pub fn max_response_bytes(&self) -> usize {
        self.values[3] as usize
    }
    pub fn max_param_bytes(&self) -> usize {
        self.values[4] as usize
    }
    pub fn max_operations(&self) -> usize {
        self.values[5] as usize
    }
    pub fn max_sql_bytes(&self) -> usize {
        self.values[6] as usize
    }

    /// Non-secret limits that change SQL semantics; part of the conversation
    /// drift fingerprint. Pool sizing is intentionally excluded.
    pub fn definition(&self) -> serde_json::Value {
        serde_json::json!({
            "statement_timeout_ms": self.statement_timeout_ms(),
            "max_rows": self.max_rows(),
            "max_response_bytes": self.max_response_bytes(),
            "max_param_bytes": self.max_param_bytes(),
        })
    }

    pub(crate) fn connect_settings(&self) -> PgConnectSettings {
        PgConnectSettings {
            max_connections: self.max_connections(),
            acquire_timeout: Duration::from_secs(30),
            retries: 3,
            backoff_base_ms: 500,
            backoff_cap_ms: 5_000,
        }
    }
}
