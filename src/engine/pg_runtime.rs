#![cfg(feature = "postgres")]
//! Shared low-level PostgreSQL plumbing (IC-040 groundwork).
//!
//! Used by both the internal `PostgresStore` and the flow-facing `AppDb`.
//! Deliberately tiny: pool construction with bounded retry/backoff and the
//! server-version floor. Nothing above the pool (fencing, idempotency, HITL)
//! belongs here.

use std::time::Duration;

use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Row};

use crate::utils::error::{IronCrewError, Result};

pub(crate) struct PgConnectSettings {
    pub max_connections: u32,
    pub acquire_timeout: Duration,
    /// Retries *after* the initial attempt.
    pub retries: u32,
    pub backoff_base_ms: u64,
    pub backoff_cap_ms: u64,
}

/// Exponential backoff delay before the next connection retry.
///
/// `attempt` is 1-based (1 = delay before the first retry). The delay doubles
/// each attempt, starting from `base_ms`, capped at `cap_ms`. Saturating math
/// keeps large attempt counts from overflowing.
pub(crate) fn retry_backoff(attempt: u32, base_ms: u64, cap_ms: u64) -> Duration {
    let factor = 1u64
        .checked_shl(attempt.saturating_sub(1))
        .unwrap_or(u64::MAX);
    Duration::from_millis(base_ms.saturating_mul(factor).min(cap_ms))
}

/// Connect a pool with bounded retry/backoff. `label` names the consumer in
/// logs ("state store" / "app database") — never the URL, which may carry
/// credentials.
pub(crate) async fn connect_pool(
    database_url: &str,
    settings: &PgConnectSettings,
    label: &str,
) -> Result<PgPool> {
    let mut attempt: u32 = 0;
    loop {
        attempt += 1;
        match PgPoolOptions::new()
            .max_connections(settings.max_connections)
            .acquire_timeout(settings.acquire_timeout)
            .connect(database_url)
            .await
        {
            Ok(pool) => return Ok(pool),
            Err(e) => {
                if attempt > settings.retries {
                    return Err(IronCrewError::Validation(format!(
                        "Failed to connect to PostgreSQL ({label}) after {attempt} attempt(s): {e}"
                    )));
                }
                let delay =
                    retry_backoff(attempt, settings.backoff_base_ms, settings.backoff_cap_ms);
                tracing::warn!(
                    "PostgreSQL ({label}) connection attempt {}/{} failed: {}; retrying in {:?}",
                    attempt,
                    settings.retries + 1,
                    e,
                    delay
                );
                tokio::time::sleep(delay).await;
            }
        }
    }
}

pub(crate) async fn ensure_supported_postgres_version(pool: &PgPool) -> Result<()> {
    let version_str: String = sqlx::query("SHOW server_version_num")
        .fetch_one(pool)
        .await
        .map_err(|e| {
            IronCrewError::Validation(format!(
                "Failed to determine PostgreSQL server version: {}",
                e
            ))
        })?
        .try_get(0)
        .map_err(|e| IronCrewError::Validation(format!("Invalid PostgreSQL version row: {}", e)))?;

    let version_num: i32 = version_str.parse().map_err(|e| {
        IronCrewError::Validation(format!(
            "Failed to parse PostgreSQL server_version_num '{}': {}",
            version_str, e
        ))
    })?;

    if version_num < 150000 {
        return Err(IronCrewError::Validation(format!(
            "PostgreSQL 15+ is required; connected server reports version {}. \
IronCrew relies on PostgreSQL 15 features for flow-scoped session uniqueness \
and targets extension-capable deployments such as pgvector-enabled installs.",
            version_str
        )));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_and_caps() {
        assert_eq!(
            retry_backoff(1, 1_000, 30_000),
            Duration::from_millis(1_000)
        );
        assert_eq!(
            retry_backoff(2, 1_000, 30_000),
            Duration::from_millis(2_000)
        );
        assert_eq!(
            retry_backoff(6, 1_000, 30_000),
            Duration::from_millis(30_000)
        );
        assert_eq!(
            retry_backoff(64, 1_000, 30_000),
            Duration::from_millis(30_000)
        );
    }
}
