#![cfg(feature = "postgres")]

use std::time::Duration;

use async_trait::async_trait;
use sqlx::{PgPool, Row};
use tokio::sync::Semaphore;

use crate::utils::error::{IronCrewError, Result};

mod audit;
mod cancellation;
mod codecs;
mod connection;
mod conversations;
mod human_input;
mod idempotency;
mod lease_helpers;
mod maintenance;
mod run_events;
mod run_history;
mod schema;
mod state_store;

use codecs::parse_timestamp;
use human_input::validate_human_input_route;

/// Upper bound on the per-retry backoff delay during store init.
const CONNECT_BACKOFF_CAP_MS: u64 = 30_000;
const MAX_DB_POOL_SIZE: u32 = 128;
const MAX_CONNECT_RETRIES: u32 = 100;
const MAX_CONNECT_TIMEOUT_SECS: u64 = 120;
const MAX_TABLE_PREFIX_BYTES: usize = 37;
const MAX_DURABLE_HUMAN_INPUT_ROWS: usize = 256;
const HUMAN_INPUT_AEAD_OVERHEAD_BYTES: usize = 28;
const DEFAULT_HUMAN_INPUT_READ_CONCURRENCY: usize = 8;
const MAX_HUMAN_INPUT_READ_CONCURRENCY: usize = 64;
const HUMAN_INPUT_READ_CONCURRENCY_ENV: &str = "IRONCREW_HITL_PG_MAX_CONCURRENT_READS";
const MIN_ACCOUNTED_RUN_EVENT_BYTES: i64 = 1024;
const MAX_EVICTED_RUN_EVENTS_PER_APPEND: u64 = 65_536;
const RUN_RECONCILIATION_BATCH_SIZE: i64 = 64;
const RUN_EVENT_SCHEMA_VERSION: i32 = 1;
fn validate_table_prefix(table_prefix: &str) -> Result<()> {
    if table_prefix.len() > MAX_TABLE_PREFIX_BYTES
        || !table_prefix
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
    {
        return Err(IronCrewError::Validation(format!(
            "Invalid IRONCREW_PG_TABLE_PREFIX '{}': maximum {MAX_TABLE_PREFIX_BYTES} lowercase ASCII alphanumeric/underscore bytes",
            table_prefix
        )));
    }
    Ok(())
}

fn parse_env<T>(name: &str, default: T) -> Result<T>
where
    T: std::str::FromStr,
{
    match std::env::var(name) {
        Ok(value) => value
            .parse::<T>()
            .map_err(|_| IronCrewError::Validation(format!("{name} has an invalid numeric value"))),
        Err(std::env::VarError::NotPresent) => Ok(default),
        Err(std::env::VarError::NotUnicode(_)) => Err(IronCrewError::Validation(format!(
            "{name} must be valid UTF-8"
        ))),
    }
}

use super::human_input::{
    DurableHumanInputRegistration, HumanInputAnswerOutcome, HumanInputKeyring,
    HumanInputListOutcome, HumanInputReadOutcome, HumanInputRegistrationOutcome,
};
use super::idempotency::{
    ConversationIdempotencyCommit, IdempotencyClaim, IdempotencyClaimOutcome,
    IdempotencyCompletion, IdempotencyCompletionOutcome, IdempotencyLimits, IdempotencyLookup,
    IdempotencyState, IdempotencyUsage, PrincipalId, RUN_OPERATION, RunCancellationRequest,
    RunFenceHeartbeat,
};
use super::input_bridge::{max_pending, max_pending_bytes};
use super::run_events::{
    EventJournalScope, HARD_MAX_EVENT_BYTES, RunEventAppendBatch, RunEventAppendOutcome,
    RunEventJournalConfig, RunEventPage,
};
use super::run_history::{
    ListRunsFilter, RunCompletion, RunIntent, RunRecord, RunStatus, RunSummary, RunTransition,
};
use super::sessions::{ConversationRecord, ConversationSummary, DialogStateRecord};
use super::store::{ConversationCoordinationScope, RunLeaseConfig, StateStore};
use super::store_sql::SqlParam;

/// Fold the shared builder's ordered params onto a sqlx query via `.bind`.
/// The `success` filter is bound as a native `bool`, matching the `BOOLEAN`
/// `success` column on the `audit_events` table.
fn bind_params<'q>(
    mut query: sqlx::query::Query<'q, sqlx::Postgres, sqlx::postgres::PgArguments>,
    params: &'q [SqlParam],
) -> sqlx::query::Query<'q, sqlx::Postgres, sqlx::postgres::PgArguments> {
    for p in params {
        query = match p {
            SqlParam::Text(s) => query.bind(s),
            SqlParam::Bool(b) => query.bind(b),
        };
    }
    query
}

pub struct PostgresStore {
    pool: PgPool,
    table_name: String,
    conversations_table: String,
    dialogs_table: String,
    audit_events_table: String,
    idempotency_table: String,
    idempotency_accounting_table: String,
    human_inputs_table: String,
    run_events_table: String,
    run_event_state_table: String,
    run_event_usage_table: String,
    human_input_keyring: Option<HumanInputKeyring>,
    human_input_max_pending_rows: usize,
    human_input_max_pending_ciphertext_bytes: usize,
    human_input_read_slots: Semaphore,
    run_event_journal_config: RunEventJournalConfig,
    lease: RunLeaseConfig,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_prefix_is_lowercase_and_identifier_safe() {
        assert!(validate_table_prefix("").is_ok());
        assert!(validate_table_prefix("project_42_").is_ok());
        assert!(validate_table_prefix(&"a".repeat(MAX_TABLE_PREFIX_BYTES)).is_ok());
        assert!(validate_table_prefix(&"a".repeat(MAX_TABLE_PREFIX_BYTES + 1)).is_err());
        assert!(validate_table_prefix("MixedCase_").is_err());
        assert!(validate_table_prefix("hyphen-").is_err());
    }
}
