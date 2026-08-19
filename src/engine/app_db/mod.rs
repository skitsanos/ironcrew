#![cfg(feature = "postgres")]
//! Flow-facing PostgreSQL app-data capability (`postgres.*`).
//!
//! Runs *named*, project-declared SQL operations on a dedicated pool that is
//! completely separate from the internal `StateStore` (own URL, role, schema).
//! See docs/postgres-app-data.md for the trust model: named operations bound
//! prompt-injected agents and reviewers, the database role bounds flow authors.

use sqlx::PgPool;
use tokio::sync::OnceCell;

use self::operations::OperationRegistry;
use self::policy::AppDbPolicy;
use crate::utils::error::{IronCrewError, Result};

pub mod operations;
pub mod policy;

mod execute;
mod sql_split;

#[cfg(test)]
mod operations_tests;
#[cfg(test)]
mod policy_tests;
#[cfg(test)]
mod split_tests;

/// Flow-facing app database. Own URL/pool/role — never the StateStore's.
/// The pool connects lazily on first use so flows that never call
/// `postgres.*` pay nothing.
#[allow(dead_code)] // consumed by src/lua/postgres.rs (Task 6)
pub struct AppDb {
    url: String,
    policy: AppDbPolicy,
    registry: OperationRegistry,
    pool: OnceCell<PgPool>,
}

impl AppDb {
    #[allow(dead_code)] // consumed by src/lua/postgres.rs (Task 6)
    pub fn new(url: String, policy: AppDbPolicy, registry: OperationRegistry) -> Self {
        Self {
            url,
            policy,
            registry,
            pool: OnceCell::new(),
        }
    }

    #[allow(dead_code)] // consumed by src/lua/postgres.rs (Task 6)
    pub fn policy(&self) -> &AppDbPolicy {
        &self.policy
    }

    #[allow(dead_code)] // consumed by src/lua/postgres.rs (Task 6)
    pub fn operation(&self, name: &str) -> Result<&operations::Operation> {
        self.registry.get(name).ok_or_else(|| {
            IronCrewError::Validation(if self.registry.is_empty() {
                format!(
                    "unknown postgres operation '{name}' (no sql/ operations defined in this project)"
                )
            } else {
                format!("unknown postgres operation '{name}'")
            })
        })
    }

    #[allow(dead_code)] // consumed by src/lua/postgres.rs (Task 6)
    pub fn definition(&self) -> serde_json::Value {
        serde_json::json!({
            "policy": self.policy.definition(),
            "operations": self.registry.definition(),
        })
    }

    async fn pool(&self) -> Result<&PgPool> {
        self.pool
            .get_or_try_init(|| async {
                let pool = crate::engine::pg_runtime::connect_pool(
                    &self.url,
                    &self.policy.connect_settings(),
                    "app database",
                )
                .await?;
                crate::engine::pg_runtime::ensure_supported_postgres_version(&pool).await?;
                Ok::<PgPool, IronCrewError>(pool)
            })
            .await
    }

    #[allow(dead_code)] // consumed by src/lua/postgres.rs (Task 6)
    pub async fn execute(&self, name: &str, params: &[serde_json::Value]) -> Result<u64> {
        execute::run_execute(self, name, params).await
    }

    #[allow(dead_code)] // consumed by src/lua/postgres.rs (Task 6)
    pub async fn query(
        &self,
        name: &str,
        params: &[serde_json::Value],
    ) -> Result<Vec<serde_json::Value>> {
        execute::run_query(self, name, params).await
    }

    #[allow(dead_code)] // consumed by src/lua/postgres.rs (Task 6)
    pub async fn query_one(
        &self,
        name: &str,
        params: &[serde_json::Value],
    ) -> Result<Option<serde_json::Value>> {
        let mut rows = execute::run_query_bounded(self, name, params, 2).await?;
        match rows.len() {
            0 => Ok(None),
            1 => Ok(Some(rows.remove(0))),
            _ => Err(IronCrewError::Validation(format!(
                "postgres operation '{name}': query_one matched more than one row"
            ))),
        }
    }
}
