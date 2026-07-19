#![cfg(feature = "postgres")]

use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

use async_trait::async_trait;
use serde::de::DeserializeOwned;
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Row};
use tokio::sync::Semaphore;

use crate::utils::error::{IronCrewError, Result};

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
const RUN_EVENT_SCHEMA_VERSION: i32 = 1;
const IDEMPOTENCY_COLUMNS: &str = "key_hash, principal_id, request_fingerprint, operation, scope, \
    resource_id, exclusive_scope, attempt_id, owner_instance_id, base_revision, state, \
    response_status, response_body, lease_expires_at, created_at, updated_at, completed_at, \
    expires_at, ttl_seconds";

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

fn decode_stored_json<T: DeserializeOwned>(raw: &str, field: &str) -> Result<T> {
    serde_json::from_str(raw).map_err(|error| {
        IronCrewError::Validation(format!(
            "PostgreSQL stored JSON in '{field}' has an invalid shape: {error}"
        ))
    })
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

fn parse_timestamp(label: &str, value: &str) -> Result<chrono::DateTime<chrono::Utc>> {
    chrono::DateTime::parse_from_rfc3339(value)
        .map(|timestamp| timestamp.with_timezone(&chrono::Utc))
        .map_err(|error| {
            IronCrewError::Validation(format!("{label} is not valid RFC3339: {error}"))
        })
}

fn canonical_timestamp(label: &str, value: &str) -> Result<String> {
    Ok(parse_timestamp(label, value)?.to_rfc3339_opts(chrono::SecondsFormat::Micros, true))
}

/// Exponential backoff delay before the next connection retry.
///
/// `attempt` is 1-based (1 = delay before the first retry). The delay doubles
/// each attempt, starting from `base_ms`, capped at `cap_ms`. Saturating math
/// keeps large attempt counts from overflowing.
fn retry_backoff(attempt: u32, base_ms: u64, cap_ms: u64) -> Duration {
    let factor = 1u64
        .checked_shl(attempt.saturating_sub(1))
        .unwrap_or(u64::MAX);
    Duration::from_millis(base_ms.saturating_mul(factor).min(cap_ms))
}

use super::human_input::{
    DurableHumanInputQuestion, DurableHumanInputRegistration, HumanInputAad,
    HumanInputAnswerOutcome, HumanInputKeyring, HumanInputListOutcome, HumanInputReadOutcome,
    HumanInputRegistrationOutcome, question_digest, validate_durable_answer,
};
use super::idempotency::{
    CONVERSATION_MESSAGE_OPERATION, ConversationIdempotencyCommit, IdempotencyClaim,
    IdempotencyClaimOutcome, IdempotencyCompletion, IdempotencyCompletionOutcome,
    IdempotencyLimits, IdempotencyLookup, IdempotencyQuotaResource, IdempotencyQuotaScope,
    IdempotencyRecord, IdempotencyState, IdempotencyUsage, PrincipalId, RUN_OPERATION,
    RunCancellationRequest, RunFenceHeartbeat, validate_digest,
};
use super::input_bridge::{max_pending, max_pending_bytes};
use super::run_events::{
    EventJournalScope, HARD_MAX_EVENT_BYTES, RunEventAppendBatch, RunEventAppendOutcome,
    RunEventBounds, RunEventEntry, RunEventGap, RunEventGapReason, RunEventJournalConfig,
    RunEventPage, RunEventTerminalState,
};
use super::run_history::{
    ListRunsFilter, RunCompletion, RunIntent, RunRecord, RunStatus, RunSummary, RunTransition,
};
use super::sessions::{ConversationRecord, ConversationSummary, DialogStateRecord};
use super::store::{RunLeaseConfig, StateStore};
use super::store_sql::{self, Dialect, SqlParam, WhereClause};

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

fn validate_human_input_route(label: &str, value: &str, max_bytes: usize) -> Result<()> {
    if value.is_empty() || value.len() > max_bytes || value.chars().any(char::is_control) {
        return Err(IronCrewError::Validation(format!(
            "Human-input {label} must be 1..={max_bytes} printable bytes"
        )));
    }
    Ok(())
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

#[derive(Debug, Clone, Copy)]
struct IdempotencyAccounting {
    records: usize,
    in_flight: usize,
    response_bytes: usize,
}

#[derive(Debug, Clone)]
struct RunEventStateRow {
    flow: String,
    owner_instance_id: String,
    latest_sequence: u64,
    dropped_through: u64,
    retained_events: u64,
    retained_bytes: u64,
    journal_complete: bool,
    eviction_reason: Option<RunEventGapReason>,
    terminal_event_sequence: Option<u64>,
}

type RunEventStateDbRow = (
    String,
    String,
    i64,
    i64,
    i64,
    i64,
    bool,
    Option<String>,
    Option<i64>,
);

#[derive(Debug, Clone, Copy)]
struct RunEventUsageRow {
    retained_events: u64,
    retained_bytes: u64,
}

#[derive(Debug, Clone)]
struct RunEventDeleteCandidate {
    run_id: String,
    sequence: u64,
    payload_bytes: u64,
}

#[derive(Debug, Clone, Default)]
struct RunEventRunEviction {
    events: u64,
    bytes: u64,
    first_sequence: u64,
    last_sequence: u64,
    previous_dropped_through: u64,
    new_dropped_through: u64,
    reason: Option<RunEventGapReason>,
}

impl RunEventRunEviction {
    fn merge(&mut self, other: &Self) {
        if other.events == 0 {
            return;
        }
        if self.events == 0 {
            *self = other.clone();
            return;
        }
        self.events = self.events.saturating_add(other.events);
        self.bytes = self.bytes.saturating_add(other.bytes);
        self.first_sequence = self.first_sequence.min(other.first_sequence);
        self.last_sequence = self.last_sequence.max(other.last_sequence);
        self.previous_dropped_through = self
            .previous_dropped_through
            .min(other.previous_dropped_through);
        if other.new_dropped_through >= self.new_dropped_through {
            self.new_dropped_through = other.new_dropped_through;
            self.reason = other.reason;
        }
    }

    fn gap(&self) -> Option<RunEventGap> {
        let reason = self.reason?;
        if self.new_dropped_through <= self.previous_dropped_through {
            return None;
        }
        Some(RunEventGap {
            first_sequence: self.previous_dropped_through.saturating_add(1),
            last_sequence: self.new_dropped_through,
            reason,
        })
    }
}

#[derive(Debug, Default)]
struct RunEventPruneSummary {
    by_run: BTreeMap<String, RunEventRunEviction>,
}

impl RunEventPruneSummary {
    fn merge(&mut self, other: Self) {
        for (run_id, eviction) in other.by_run {
            self.by_run.entry(run_id).or_default().merge(&eviction);
        }
    }

    fn for_run(&self, run_id: &str) -> RunEventRunEviction {
        self.by_run.get(run_id).cloned().unwrap_or_default()
    }
}

fn run_event_gap_reason_db(reason: RunEventGapReason) -> &'static str {
    match reason {
        RunEventGapReason::WriterBackpressure => "writer_backpressure",
        RunEventGapReason::Retention => "retention",
        RunEventGapReason::GlobalCapacity => "global_capacity",
        RunEventGapReason::OwnerLost => "owner_lost",
    }
}

fn parse_run_event_gap_reason(value: &str) -> Result<RunEventGapReason> {
    match value {
        "writer_backpressure" => Ok(RunEventGapReason::WriterBackpressure),
        "retention" => Ok(RunEventGapReason::Retention),
        "global_capacity" => Ok(RunEventGapReason::GlobalCapacity),
        "owner_lost" => Ok(RunEventGapReason::OwnerLost),
        _ => Err(IronCrewError::Validation(format!(
            "PostgreSQL run-event state contains invalid eviction reason '{value}'"
        ))),
    }
}

fn nonnegative_u64(label: &str, value: i64) -> Result<u64> {
    u64::try_from(value).map_err(|_| {
        IronCrewError::Validation(format!(
            "PostgreSQL run-event {label} is negative or out of range"
        ))
    })
}

impl PostgresStore {
    /// Create a new PostgreSQL store.
    /// `table_prefix` allows sharing a database across projects:
    ///   prefix = "myapp_" → table = "myapp_runs"
    ///   prefix = "" → table = "runs" (default)
    pub async fn new(database_url: &str, table_prefix: &str) -> Result<Self> {
        Self::new_with_lease_config(database_url, table_prefix, RunLeaseConfig::from_env()?).await
    }

    pub async fn new_with_lease_config(
        database_url: &str,
        table_prefix: &str,
        lease: RunLeaseConfig,
    ) -> Result<Self> {
        let keyring = HumanInputKeyring::from_env()?;
        Self::new_with_lease_config_and_human_input_keyring(
            database_url,
            table_prefix,
            lease,
            keyring,
        )
        .await
    }

    /// Construct a store with an explicit durable human-input keyring.
    ///
    /// Production callers normally use [`Self::new_with_lease_config`], which
    /// loads the keyring once from the environment. This constructor keeps
    /// live-database tests deterministic and avoids process-global env races.
    pub async fn new_with_lease_config_and_human_input_keyring(
        database_url: &str,
        table_prefix: &str,
        lease: RunLeaseConfig,
        human_input_keyring: Option<HumanInputKeyring>,
    ) -> Result<Self> {
        let run_event_journal_config = RunEventJournalConfig::from_env()?;
        Self::new_with_runtime_config(
            database_url,
            table_prefix,
            lease,
            human_input_keyring,
            run_event_journal_config,
        )
        .await
    }

    /// Construct a store with deterministic process-wide runtime features.
    /// Production constructors load these immutable values once from env;
    /// live PostgreSQL tests use this entrypoint to avoid environment races.
    pub async fn new_with_runtime_config(
        database_url: &str,
        table_prefix: &str,
        lease: RunLeaseConfig,
        human_input_keyring: Option<HumanInputKeyring>,
        run_event_journal_config: RunEventJournalConfig,
    ) -> Result<Self> {
        // Validate table prefix to prevent SQL injection via env var
        validate_table_prefix(table_prefix)?;
        run_event_journal_config.validate()?;
        let human_input_max_pending_rows = max_pending().min(MAX_DURABLE_HUMAN_INPUT_ROWS);
        let human_input_max_pending_ciphertext_bytes = max_pending_bytes()
            .checked_add(
                human_input_max_pending_rows.saturating_mul(HUMAN_INPUT_AEAD_OVERHEAD_BYTES),
            )
            .ok_or_else(|| {
                IronCrewError::Validation("PostgreSQL human-input ciphertext limit overflow".into())
            })?;
        let human_input_read_concurrency: usize = parse_env(
            HUMAN_INPUT_READ_CONCURRENCY_ENV,
            DEFAULT_HUMAN_INPUT_READ_CONCURRENCY,
        )?;
        if !(1..=MAX_HUMAN_INPUT_READ_CONCURRENCY).contains(&human_input_read_concurrency) {
            return Err(IronCrewError::Validation(format!(
                "{HUMAN_INPUT_READ_CONCURRENCY_ENV} must be between 1 and {MAX_HUMAN_INPUT_READ_CONCURRENCY}"
            )));
        }

        let max_conn: u32 = parse_env("IRONCREW_DB_POOL_SIZE", 10)?;
        if max_conn == 0 || max_conn > MAX_DB_POOL_SIZE {
            return Err(IronCrewError::Validation(format!(
                "IRONCREW_DB_POOL_SIZE must be between 1 and {MAX_DB_POOL_SIZE}"
            )));
        }

        // Retries *after* the initial attempt. With backoff this rides out a
        // transient database outage (e.g. a platform restart) so a brief blip
        // doesn't crash the process and burn a container restart per attempt.
        let retries: u32 = parse_env("IRONCREW_DB_CONNECT_RETRIES", 10)?;
        if retries > MAX_CONNECT_RETRIES {
            return Err(IronCrewError::Validation(format!(
                "IRONCREW_DB_CONNECT_RETRIES must be at most {MAX_CONNECT_RETRIES}"
            )));
        }
        let backoff_base_ms: u64 = parse_env("IRONCREW_DB_CONNECT_BACKOFF_MS", 1_000)?;
        if backoff_base_ms == 0 || backoff_base_ms > CONNECT_BACKOFF_CAP_MS {
            return Err(IronCrewError::Validation(format!(
                "IRONCREW_DB_CONNECT_BACKOFF_MS must be between 1 and {CONNECT_BACKOFF_CAP_MS}"
            )));
        }
        let connect_timeout_secs: u64 = parse_env("IRONCREW_DB_CONNECT_TIMEOUT_SECS", 30)?;
        if connect_timeout_secs == 0 || connect_timeout_secs > MAX_CONNECT_TIMEOUT_SECS {
            return Err(IronCrewError::Validation(format!(
                "IRONCREW_DB_CONNECT_TIMEOUT_SECS must be between 1 and {MAX_CONNECT_TIMEOUT_SECS}"
            )));
        }

        let mut attempt: u32 = 0;
        let pool = loop {
            attempt += 1;
            match PgPoolOptions::new()
                .max_connections(max_conn)
                .acquire_timeout(Duration::from_secs(connect_timeout_secs))
                .connect(database_url)
                .await
            {
                Ok(pool) => break pool,
                Err(e) => {
                    if attempt > retries {
                        return Err(IronCrewError::Validation(format!(
                            "Failed to connect to PostgreSQL after {} attempt(s): {}",
                            attempt, e
                        )));
                    }
                    let delay = retry_backoff(attempt, backoff_base_ms, CONNECT_BACKOFF_CAP_MS);
                    tracing::warn!(
                        "PostgreSQL connection attempt {}/{} failed: {}; retrying in {:?}",
                        attempt,
                        retries + 1,
                        e,
                        delay
                    );
                    tokio::time::sleep(delay).await;
                }
            }
        };

        ensure_supported_postgres_version(&pool).await?;

        let table_name = format!("{}runs", table_prefix);
        let conversations_table = format!("{}conversations", table_prefix);
        let dialogs_table = format!("{}dialogs", table_prefix);
        let audit_events_table = format!("{}audit_events", table_prefix);
        let idempotency_table = format!("{}idempotency", table_prefix);
        let idempotency_accounting_table = format!("{}idempotency_accounting", table_prefix);
        let human_inputs_table = format!("{}human_inputs", table_prefix);
        let run_events_table = format!("{}run_events", table_prefix);
        let run_event_state_table = format!("{}run_event_state", table_prefix);
        let run_event_usage_table = format!("{}run_event_usage", table_prefix);

        let store = Self {
            pool,
            table_name: table_name.clone(),
            conversations_table,
            dialogs_table,
            audit_events_table,
            idempotency_table,
            idempotency_accounting_table,
            human_inputs_table,
            run_events_table,
            run_event_state_table,
            run_event_usage_table,
            human_input_keyring,
            human_input_max_pending_rows,
            human_input_max_pending_ciphertext_bytes,
            human_input_read_slots: Semaphore::new(human_input_read_concurrency),
            run_event_journal_config,
            lease,
        };
        store.bootstrap().await?;

        tracing::info!("PostgreSQL store ready (table: {})", table_name);
        Ok(store)
    }

    async fn lock_advisory(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        domain: &str,
        identity: &str,
        shared: bool,
    ) -> Result<()> {
        let lock_name = format!(
            "ironcrew:{}:{domain}:{}:{identity}",
            self.idempotency_table,
            identity.len()
        );
        let sql = if shared {
            "SELECT pg_advisory_xact_lock_shared(hashtextextended($1, 0))"
        } else {
            "SELECT pg_advisory_xact_lock(hashtextextended($1, 0))"
        };
        sqlx::query(sql)
            .bind(lock_name)
            .execute(&mut **tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "Failed to acquire PostgreSQL {domain} advisory lock: {error}"
                ))
            })?;
        Ok(())
    }

    async fn lock_idempotency_quota(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    ) -> Result<()> {
        self.lock_advisory(tx, "idempotency-quota", "global", false)
            .await
    }

    async fn lock_idempotency_key(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        key_hash: &str,
    ) -> Result<()> {
        self.lock_advisory(tx, "idempotency-key", key_hash, false)
            .await
    }

    async fn lock_idempotency_principal(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        principal_id: &PrincipalId,
    ) -> Result<()> {
        self.lock_advisory(tx, "idempotency-principal", principal_id.as_str(), false)
            .await
    }

    async fn lock_idempotency_scope(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        exclusive_scope: &str,
    ) -> Result<()> {
        self.lock_advisory(tx, "idempotency-scope", exclusive_scope, false)
            .await
    }

    async fn lock_resource(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        operation: &str,
        scope: &str,
        resource_id: &str,
    ) -> Result<()> {
        let identity = format!(
            "{}:{operation}:{}:{scope}:{}:{resource_id}",
            operation.len(),
            scope.len(),
            resource_id.len()
        );
        self.lock_advisory(tx, "resource", &identity, false).await
    }

    async fn lock_run_fence(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        shared: bool,
    ) -> Result<()> {
        self.lock_advisory(tx, "run-fence", "global", shared).await
    }

    async fn delete_human_inputs_for_run(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        run_id: &str,
    ) -> Result<u64> {
        let sql = format!("DELETE FROM {} WHERE run_id = $1", self.human_inputs_table);
        let deleted = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(run_id)
            .execute(&mut **tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL human-input mailbox cleanup failed: {error}"
                ))
            })?;
        Ok(deleted.rows_affected())
    }

    async fn lock_run_event_usage(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    ) -> Result<RunEventUsageRow> {
        let sql = format!(
            "SELECT retained_events, retained_bytes FROM {} \
             WHERE singleton = TRUE FOR UPDATE",
            self.run_event_usage_table
        );
        let row: Option<(i64, i64)> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
            .fetch_optional(&mut **tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL run-event global accounting lock failed: {error}"
                ))
            })?;
        let (retained_events, retained_bytes) = row.ok_or_else(|| {
            IronCrewError::Validation(
                "PostgreSQL run-event global accounting row is missing".into(),
            )
        })?;
        Ok(RunEventUsageRow {
            retained_events: nonnegative_u64("global retained event count", retained_events)?,
            retained_bytes: nonnegative_u64("global retained byte count", retained_bytes)?,
        })
    }

    async fn run_event_state_for_update(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        run_id: &str,
    ) -> Result<Option<RunEventStateRow>> {
        self.run_event_state(tx, run_id, "FOR UPDATE").await
    }

    async fn run_event_state_for_share(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        run_id: &str,
    ) -> Result<Option<RunEventStateRow>> {
        self.run_event_state(tx, run_id, "FOR SHARE").await
    }

    async fn run_event_state(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        run_id: &str,
        lock: &str,
    ) -> Result<Option<RunEventStateRow>> {
        let sql = format!(
            "SELECT flow, owner_instance_id, latest_sequence, dropped_through, \
                    retained_events, retained_bytes, journal_complete, \
                    eviction_reason, terminal_event_sequence \
             FROM {} WHERE run_id = $1 {lock}",
            self.run_event_state_table,
        );
        let row: Option<RunEventStateDbRow> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
            .bind(run_id)
            .fetch_optional(&mut **tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL run-event state lookup failed: {error}"
                ))
            })?;
        row.map(
            |(
                flow,
                owner_instance_id,
                latest_sequence,
                dropped_through,
                retained_events,
                retained_bytes,
                journal_complete,
                eviction_reason,
                terminal_event_sequence,
            )| {
                Ok(RunEventStateRow {
                    flow,
                    owner_instance_id,
                    latest_sequence: nonnegative_u64("latest sequence", latest_sequence)?,
                    dropped_through: nonnegative_u64("dropped boundary", dropped_through)?,
                    retained_events: nonnegative_u64(
                        "per-run retained event count",
                        retained_events,
                    )?,
                    retained_bytes: nonnegative_u64("per-run retained byte count", retained_bytes)?,
                    journal_complete,
                    eviction_reason: eviction_reason
                        .as_deref()
                        .map(parse_run_event_gap_reason)
                        .transpose()?,
                    terminal_event_sequence: terminal_event_sequence
                        .map(|value| nonnegative_u64("terminal event sequence", value))
                        .transpose()?,
                })
            },
        )
        .transpose()
    }

    async fn delete_run_event_candidates(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        candidates: &[RunEventDeleteCandidate],
        reason: RunEventGapReason,
    ) -> Result<RunEventPruneSummary> {
        if candidates.is_empty() {
            return Ok(RunEventPruneSummary::default());
        }
        let run_ids: Vec<String> = candidates
            .iter()
            .map(|candidate| candidate.run_id.clone())
            .collect();
        let sequences: Vec<i64> = candidates
            .iter()
            .map(|candidate| {
                i64::try_from(candidate.sequence).map_err(|_| {
                    IronCrewError::Validation("Run-event sequence exceeds PostgreSQL BIGINT".into())
                })
            })
            .collect::<Result<_>>()?;
        let sql = format!(
            "DELETE FROM {events} AS event USING (\
                 SELECT * FROM unnest($1::text[], $2::bigint[]) \
                 AS selected(run_id, sequence)\
             ) AS selected \
             WHERE event.run_id = selected.run_id \
               AND event.sequence = selected.sequence \
             RETURNING event.run_id, event.sequence, event.accounted_bytes AS payload_bytes",
            events = self.run_events_table
        );
        let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(&run_ids)
            .bind(&sequences)
            .fetch_all(&mut **tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL run-event bounded prune failed: {error}"
                ))
            })?;
        if rows.len() != candidates.len() {
            return Err(IronCrewError::Conflict(
                "Run-event rows changed during bounded pruning".into(),
            ));
        }

        let mut deleted_by_run: BTreeMap<String, RunEventRunEviction> = BTreeMap::new();
        for row in rows {
            let run_id: String = row.try_get("run_id").map_err(|error| {
                IronCrewError::Validation(format!("Run-event run_id column: {error}"))
            })?;
            let sequence = nonnegative_u64(
                "deleted sequence",
                row.try_get::<i64, _>("sequence").map_err(|error| {
                    IronCrewError::Validation(format!("Run-event sequence column: {error}"))
                })?,
            )?;
            let payload_bytes = nonnegative_u64(
                "deleted payload byte count",
                row.try_get::<i64, _>("payload_bytes").map_err(|error| {
                    IronCrewError::Validation(format!("Run-event payload_bytes column: {error}"))
                })?,
            )?;
            let eviction = deleted_by_run.entry(run_id).or_default();
            eviction.events = eviction.events.saturating_add(1);
            eviction.bytes = eviction.bytes.saturating_add(payload_bytes);
            if eviction.first_sequence == 0 {
                eviction.first_sequence = sequence;
            } else {
                eviction.first_sequence = eviction.first_sequence.min(sequence);
            }
            eviction.last_sequence = eviction.last_sequence.max(sequence);
        }

        for (run_id, eviction) in &mut deleted_by_run {
            let state = self
                .run_event_state_for_update(tx, run_id)
                .await?
                .ok_or_else(|| {
                    IronCrewError::Validation(format!(
                        "PostgreSQL run-event state for '{run_id}' is missing"
                    ))
                })?;
            eviction.previous_dropped_through = state.dropped_through;
            let retained_events = state
                .retained_events
                .checked_sub(eviction.events)
                .ok_or_else(|| {
                    IronCrewError::Validation(format!(
                        "PostgreSQL run-event count accounting underflow for '{run_id}'"
                    ))
                })?;
            let retained_bytes = state
                .retained_bytes
                .checked_sub(eviction.bytes)
                .ok_or_else(|| {
                    IronCrewError::Validation(format!(
                        "PostgreSQL run-event byte accounting underflow for '{run_id}'"
                    ))
                })?;
            let earliest_sql = format!(
                "SELECT MIN(sequence) FROM {} WHERE run_id = $1",
                self.run_events_table
            );
            let earliest: Option<i64> = sqlx::query_scalar(sqlx::AssertSqlSafe(earliest_sql))
                .bind(run_id)
                .fetch_one(&mut **tx)
                .await
                .map_err(|error| {
                    IronCrewError::Validation(format!(
                        "PostgreSQL run-event retained-bound lookup failed: {error}"
                    ))
                })?;
            let new_dropped_through = if retained_events == 0 {
                state.latest_sequence
            } else {
                let earliest = earliest
                    .ok_or_else(|| {
                        IronCrewError::Validation(
                            "PostgreSQL run-event accounting retained rows are missing".into(),
                        )
                    })
                    .and_then(|value| nonnegative_u64("earliest retained sequence", value))?;
                state.dropped_through.max(earliest.saturating_sub(1))
            };
            eviction.new_dropped_through = new_dropped_through;
            if new_dropped_through > state.dropped_through {
                eviction.reason = Some(reason);
            }
            let eviction_reason = if new_dropped_through > state.dropped_through {
                Some(run_event_gap_reason_db(reason))
            } else {
                state.eviction_reason.map(run_event_gap_reason_db)
            };
            let journal_complete =
                state.journal_complete && eviction.last_sequence <= new_dropped_through;
            let update_sql = format!(
                "UPDATE {} SET retained_events = $1, retained_bytes = $2, \
                     dropped_through = $3, eviction_reason = $4, \
                     journal_complete = $5, updated_at = clock_timestamp() \
                 WHERE run_id = $6",
                self.run_event_state_table
            );
            let updated = sqlx::query(sqlx::AssertSqlSafe(update_sql))
                .bind(i64::try_from(retained_events).map_err(|_| {
                    IronCrewError::Validation("Run-event retained count exceeds BIGINT".into())
                })?)
                .bind(i64::try_from(retained_bytes).map_err(|_| {
                    IronCrewError::Validation("Run-event retained bytes exceed BIGINT".into())
                })?)
                .bind(i64::try_from(new_dropped_through).map_err(|_| {
                    IronCrewError::Validation("Run-event dropped boundary exceeds BIGINT".into())
                })?)
                .bind(eviction_reason)
                .bind(journal_complete)
                .bind(run_id)
                .execute(&mut **tx)
                .await
                .map_err(|error| {
                    IronCrewError::Validation(format!(
                        "PostgreSQL run-event state prune update failed: {error}"
                    ))
                })?;
            if updated.rows_affected() != 1 {
                return Err(IronCrewError::Conflict(format!(
                    "Run-event state for '{run_id}' changed during pruning"
                )));
            }
        }

        Ok(RunEventPruneSummary {
            by_run: deleted_by_run,
        })
    }

    async fn prune_expired_run_events(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    ) -> Result<RunEventPruneSummary> {
        let sql = format!(
            "SELECT event.run_id, event.sequence, \
                    event.accounted_bytes AS payload_bytes \
             FROM {events} AS event \
             WHERE event.expires_at <= clock_timestamp() \
             ORDER BY event.expires_at, event.run_id, event.sequence \
             LIMIT $1 FOR UPDATE OF event",
            events = self.run_events_table
        );
        let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(
                i64::try_from(self.run_event_journal_config.prune_batch).map_err(|_| {
                    IronCrewError::Validation("Run-event prune batch exceeds BIGINT".into())
                })?,
            )
            .fetch_all(&mut **tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL expired run-event selection failed: {error}"
                ))
            })?;
        let candidates = rows
            .into_iter()
            .map(|row| {
                Ok(RunEventDeleteCandidate {
                    run_id: row.try_get("run_id").map_err(|error| {
                        IronCrewError::Validation(format!("Run-event run_id column: {error}"))
                    })?,
                    sequence: nonnegative_u64(
                        "expired sequence",
                        row.try_get("sequence").map_err(|error| {
                            IronCrewError::Validation(format!("Run-event sequence column: {error}"))
                        })?,
                    )?,
                    payload_bytes: nonnegative_u64(
                        "expired payload byte count",
                        row.try_get("payload_bytes").map_err(|error| {
                            IronCrewError::Validation(format!(
                                "Run-event payload_bytes column: {error}"
                            ))
                        })?,
                    )?,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        self.delete_run_event_candidates(tx, &candidates, RunEventGapReason::Retention)
            .await
    }

    /// Opportunistic physical retention sweep run after core reconciliation
    /// commits. Failure or lock contention is deliberately non-fatal: logical
    /// reads already filter expired rows, and maintenance must never roll back
    /// abandonment/idempotency recovery.
    async fn prune_expired_run_events_best_effort(&self) {
        let mut tx = match self.pool.begin().await {
            Ok(tx) => tx,
            Err(error) => {
                tracing::warn!(%error, "PostgreSQL run-event maintenance transaction unavailable");
                return;
            }
        };
        let lock_name = format!("ironcrew:{}:run-event-maintenance", self.run_events_table);
        let acquired: bool =
            match sqlx::query_scalar("SELECT pg_try_advisory_xact_lock(hashtextextended($1, 0))")
                .bind(lock_name)
                .fetch_one(&mut *tx)
                .await
            {
                Ok(acquired) => acquired,
                Err(error) => {
                    let _ = tx.rollback().await;
                    tracing::warn!(%error, "PostgreSQL run-event maintenance lock probe failed");
                    return;
                }
            };
        if !acquired {
            let _ = tx.rollback().await;
            return;
        }
        if let Err(error) = sqlx::query("SET LOCAL lock_timeout = '100ms'")
            .execute(&mut *tx)
            .await
        {
            let _ = tx.rollback().await;
            tracing::warn!(%error, "PostgreSQL run-event maintenance timeout setup failed");
            return;
        }
        if let Err(error) = self.lock_run_event_usage(&mut tx).await {
            let _ = tx.rollback().await;
            tracing::debug!(%error, "PostgreSQL run-event maintenance skipped on usage contention");
            return;
        }
        if let Err(error) = self.prune_expired_run_events(&mut tx).await {
            let _ = tx.rollback().await;
            tracing::warn!(%error, "PostgreSQL run-event maintenance prune failed");
            return;
        }
        if let Err(error) = tx.commit().await {
            tracing::warn!(%error, "PostgreSQL run-event maintenance commit failed");
        }
    }

    async fn evict_run_event_capacity(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        run_id: &str,
        events_to_free: u64,
        bytes_to_free: u64,
    ) -> Result<RunEventPruneSummary> {
        if events_to_free == 0 && bytes_to_free == 0 {
            return Ok(RunEventPruneSummary::default());
        }
        let sql = format!(
            "SELECT run_id, sequence, accounted_bytes AS payload_bytes FROM {} \
             WHERE run_id = $1 ORDER BY sequence LIMIT $2 FOR UPDATE",
            self.run_events_table
        );
        let batch_limit =
            i64::try_from(self.run_event_journal_config.prune_batch).map_err(|_| {
                IronCrewError::Validation("Run-event prune batch exceeds BIGINT".into())
            })?;
        let mut remaining_events = events_to_free;
        let mut remaining_bytes = bytes_to_free;
        let mut total_evicted = 0u64;
        let mut summary = RunEventPruneSummary::default();
        while remaining_events > 0 || remaining_bytes > 0 {
            let rows = sqlx::query(sqlx::AssertSqlSafe(sql.clone()))
                .bind(run_id)
                .bind(batch_limit)
                .fetch_all(&mut **tx)
                .await
                .map_err(|error| {
                    IronCrewError::Validation(format!(
                        "PostgreSQL per-run event eviction selection failed: {error}"
                    ))
                })?;
            let mut candidates = Vec::new();
            let mut freed_bytes = 0u64;
            for row in rows {
                let candidate = RunEventDeleteCandidate {
                    run_id: row.try_get("run_id").map_err(|error| {
                        IronCrewError::Validation(format!("Run-event run_id column: {error}"))
                    })?,
                    sequence: nonnegative_u64(
                        "evicted sequence",
                        row.try_get("sequence").map_err(|error| {
                            IronCrewError::Validation(format!("Run-event sequence column: {error}"))
                        })?,
                    )?,
                    payload_bytes: nonnegative_u64(
                        "evicted payload byte count",
                        row.try_get("payload_bytes").map_err(|error| {
                            IronCrewError::Validation(format!(
                                "Run-event payload_bytes column: {error}"
                            ))
                        })?,
                    )?,
                };
                freed_bytes = freed_bytes.saturating_add(candidate.payload_bytes);
                candidates.push(candidate);
                if candidates.len() as u64 >= remaining_events && freed_bytes >= remaining_bytes {
                    break;
                }
            }
            if candidates.is_empty()
                || total_evicted.saturating_add(candidates.len() as u64)
                    > MAX_EVICTED_RUN_EVENTS_PER_APPEND
            {
                return Err(IronCrewError::Conflict(format!(
                    "Run-event per-run capacity for '{run_id}' cannot be reclaimed within the bounded {MAX_EVICTED_RUN_EVENTS_PER_APPEND}-row append budget"
                )));
            }
            let freed_events = candidates.len() as u64;
            summary.merge(
                self.delete_run_event_candidates(
                    tx,
                    &candidates,
                    RunEventGapReason::WriterBackpressure,
                )
                .await?,
            );
            total_evicted = total_evicted.saturating_add(freed_events);
            remaining_events = remaining_events.saturating_sub(freed_events);
            remaining_bytes = remaining_bytes.saturating_sub(freed_bytes);
        }
        Ok(summary)
    }

    async fn evict_global_run_event_capacity(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        events_to_free: u64,
        bytes_to_free: u64,
    ) -> Result<RunEventPruneSummary> {
        if events_to_free == 0 && bytes_to_free == 0 {
            return Ok(RunEventPruneSummary::default());
        }
        let sql = format!(
            "SELECT run_id, sequence, accounted_bytes AS payload_bytes FROM {} \
             ORDER BY created_at, run_id, sequence LIMIT $1 FOR UPDATE",
            self.run_events_table
        );
        let batch_limit =
            i64::try_from(self.run_event_journal_config.prune_batch).map_err(|_| {
                IronCrewError::Validation("Run-event prune batch exceeds BIGINT".into())
            })?;
        let mut remaining_events = events_to_free;
        let mut remaining_bytes = bytes_to_free;
        let mut total_evicted = 0u64;
        let mut summary = RunEventPruneSummary::default();
        while remaining_events > 0 || remaining_bytes > 0 {
            let rows = sqlx::query(sqlx::AssertSqlSafe(sql.clone()))
                .bind(batch_limit)
                .fetch_all(&mut **tx)
                .await
                .map_err(|error| {
                    IronCrewError::Validation(format!(
                        "PostgreSQL global event eviction selection failed: {error}"
                    ))
                })?;
            let mut candidates = Vec::new();
            let mut freed_bytes = 0u64;
            for row in rows {
                let candidate = RunEventDeleteCandidate {
                    run_id: row.try_get("run_id").map_err(|error| {
                        IronCrewError::Validation(format!("Run-event run_id column: {error}"))
                    })?,
                    sequence: nonnegative_u64(
                        "globally evicted sequence",
                        row.try_get("sequence").map_err(|error| {
                            IronCrewError::Validation(format!("Run-event sequence column: {error}"))
                        })?,
                    )?,
                    payload_bytes: nonnegative_u64(
                        "globally evicted payload byte count",
                        row.try_get("payload_bytes").map_err(|error| {
                            IronCrewError::Validation(format!(
                                "Run-event payload_bytes column: {error}"
                            ))
                        })?,
                    )?,
                };
                freed_bytes = freed_bytes.saturating_add(candidate.payload_bytes);
                candidates.push(candidate);
                if candidates.len() as u64 >= remaining_events && freed_bytes >= remaining_bytes {
                    break;
                }
            }
            if candidates.is_empty()
                || total_evicted.saturating_add(candidates.len() as u64)
                    > MAX_EVICTED_RUN_EVENTS_PER_APPEND
            {
                return Err(IronCrewError::Conflict(format!(
                    "Run-event global capacity cannot be reclaimed within the bounded {MAX_EVICTED_RUN_EVENTS_PER_APPEND}-row append budget"
                )));
            }
            let freed_events = candidates.len() as u64;
            summary.merge(
                self.delete_run_event_candidates(
                    tx,
                    &candidates,
                    RunEventGapReason::GlobalCapacity,
                )
                .await?,
            );
            total_evicted = total_evicted.saturating_add(freed_events);
            remaining_events = remaining_events.saturating_sub(freed_events);
            remaining_bytes = remaining_bytes.saturating_sub(freed_bytes);
        }
        Ok(summary)
    }

    async fn run_event_bounds(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        run_id: &str,
        state: &RunEventStateRow,
    ) -> Result<RunEventBounds> {
        let sql = format!(
            "SELECT MIN(sequence) FROM {} WHERE run_id = $1",
            self.run_events_table
        );
        let earliest: Option<i64> = sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
            .bind(run_id)
            .fetch_one(&mut **tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL run-event earliest-bound lookup failed: {error}"
                ))
            })?;
        let bounds = RunEventBounds {
            earliest_retained_sequence: earliest
                .map(|value| nonnegative_u64("earliest retained sequence", value))
                .transpose()?,
            latest_sequence: state.latest_sequence,
            dropped_through: state.dropped_through,
            retained_events: state.retained_events,
            retained_bytes: state.retained_bytes,
            journal_complete: state.journal_complete,
        };
        bounds.validate()?;
        Ok(bounds)
    }

    async fn idempotency_accounting_for_update(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        principal_id: &PrincipalId,
    ) -> Result<(IdempotencyAccounting, IdempotencyAccounting)> {
        let global_sql = format!(
            "SELECT record_count, in_flight_count, response_bytes FROM {} \
             WHERE principal_id = 'global' AND is_global = TRUE FOR UPDATE",
            self.idempotency_accounting_table
        );
        let global = sqlx::query_as(sqlx::AssertSqlSafe(global_sql))
            .fetch_optional(&mut **tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL global idempotency accounting lookup failed: {error}"
                ))
            })?
            .ok_or_else(|| {
                IronCrewError::Validation(
                    "PostgreSQL global idempotency accounting row is missing".into(),
                )
            })?;

        let principal_sql = format!(
            "SELECT record_count, in_flight_count, response_bytes FROM {} \
             WHERE principal_id = $1 AND is_global = FALSE FOR UPDATE",
            self.idempotency_accounting_table
        );
        let principal = sqlx::query_as(sqlx::AssertSqlSafe(principal_sql))
            .bind(principal_id.as_str())
            .fetch_optional(&mut **tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL principal idempotency accounting lookup failed: {error}"
                ))
            })?;
        Ok((
            decode_idempotency_accounting(global)?,
            principal
                .map(decode_idempotency_accounting)
                .transpose()?
                .unwrap_or(IdempotencyAccounting {
                    records: 0,
                    in_flight: 0,
                    response_bytes: 0,
                }),
        ))
    }

    async fn idempotency_retry_after_seconds(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        principal_id: Option<&PrincipalId>,
        resource: IdempotencyQuotaResource,
        database_now: &str,
    ) -> Result<u64> {
        let principal = principal_id.map(PrincipalId::as_str);
        let sql = match resource {
            IdempotencyQuotaResource::Records => format!(
                "SELECT GREATEST(1, COALESCE(CEIL(EXTRACT(EPOCH FROM (MIN(\
                     CASE WHEN state IN ('completed', 'indeterminate') \
                          THEN expires_at::timestamptz \
                          ELSE lease_expires_at::timestamptz + \
                               ttl_seconds * interval '1 second' END\
                 ) - $1::timestamptz)))::BIGINT, 1)) \
                 FROM {} WHERE ($2::TEXT IS NULL OR principal_id = $2)",
                self.idempotency_table
            ),
            IdempotencyQuotaResource::InFlight => format!(
                "SELECT GREATEST(1, COALESCE(CEIL(EXTRACT(EPOCH FROM (\
                     MIN(lease_expires_at::timestamptz) - $1::timestamptz\
                 )))::BIGINT, 1)) \
                 FROM {} WHERE state IN ('claimed', 'running') \
                   AND ($2::TEXT IS NULL OR principal_id = $2)",
                self.idempotency_table
            ),
        };
        let seconds: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
            .bind(database_now)
            .bind(principal)
            .fetch_one(&mut **tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL idempotency quota retry calculation failed: {error}"
                ))
            })?;
        u64::try_from(seconds).map_err(|_| {
            IronCrewError::Validation(
                "PostgreSQL idempotency quota retry delay is out of range".into(),
            )
        })
    }

    async fn get_idempotency_in_transaction(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        key_hash: &str,
    ) -> Result<Option<IdempotencyRecord>> {
        let sql = format!(
            "SELECT {IDEMPOTENCY_COLUMNS} FROM {} WHERE key_hash = $1 FOR UPDATE",
            self.idempotency_table
        );
        let row = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(key_hash)
            .fetch_optional(&mut **tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!("PostgreSQL idempotency lookup failed: {error}"))
            })?;
        row.as_ref().map(row_to_idempotency_record).transpose()
    }

    async fn idempotency_principal_for_key(&self, key_hash: &str) -> Result<Option<PrincipalId>> {
        let sql = format!(
            "SELECT principal_id FROM {} WHERE key_hash = $1",
            self.idempotency_table
        );
        let principal: Option<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
            .bind(key_hash)
            .fetch_optional(&self.pool)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL idempotency principal lookup failed: {error}"
                ))
            })?;
        principal.map(PrincipalId::from_digest).transpose()
    }

    async fn database_clock_with_deadline(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        seconds: u64,
        context: &str,
    ) -> Result<(String, String)> {
        let seconds = i64::try_from(seconds).map_err(|_| {
            IronCrewError::Validation(format!("PostgreSQL {context} duration is out of range"))
        })?;
        sqlx::query_as::<_, (String, String)>(
            "WITH db_clock AS (SELECT clock_timestamp() AS now) \
             SELECT \
                 to_char(now AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"'), \
                 to_char(\
                     (now + $1::bigint * interval '1 second') AT TIME ZONE 'UTC', \
                     'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"'\
                 ) \
             FROM db_clock",
        )
        .bind(seconds)
        .fetch_one(&mut **tx)
        .await
        .map_err(|error| {
            IronCrewError::Validation(format!(
                "Failed to read PostgreSQL clock for {context}: {error}"
            ))
        })
    }

    async fn mark_record_indeterminate_in_transaction(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        mut record: IdempotencyRecord,
    ) -> Result<IdempotencyRecord> {
        let (completed_at, expires_at) = self
            .database_clock_with_deadline(
                tx,
                record.ttl_seconds,
                "idempotency indeterminate transition",
            )
            .await?;
        let sql = format!(
            "UPDATE {} SET state = 'indeterminate', response_status = NULL, \
             response_body = NULL, lease_expires_at = '', updated_at = $1, \
             completed_at = $1, expires_at = $2 \
             WHERE key_hash = $3 AND attempt_id = $4 \
               AND state IN ('claimed', 'running')",
            self.idempotency_table
        );
        let result = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(&completed_at)
            .bind(&expires_at)
            .bind(&record.key_hash)
            .bind(&record.attempt_id)
            .execute(&mut **tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL idempotency indeterminate transition failed: {error}"
                ))
            })?;
        if result.rows_affected() != 1 {
            return Err(IronCrewError::Conflict(format!(
                "Idempotency claim '{}' changed before it could be fenced",
                record.key_hash
            )));
        }
        record.state = IdempotencyState::Indeterminate;
        record.response_status = None;
        record.response_body = None;
        record.lease_expires_at.clear();
        record.updated_at = completed_at.clone();
        record.completed_at = Some(completed_at);
        record.expires_at = Some(expires_at);
        record.validate()?;
        Ok(record)
    }

    /// Bootstrap the database: create table, add missing columns, fix types, create indexes.
    async fn bootstrap(&self) -> Result<()> {
        let t = &self.table_name;
        // Keep the entire schema transition atomic. A partial bootstrap can
        // otherwise leave a pod "ready" with missing ownership columns or
        // uniqueness guarantees after a transient DDL/permission failure.
        let mut tx = self.pool.begin().await.map_err(|e| {
            IronCrewError::Validation(format!("Failed to begin PostgreSQL bootstrap: {e}"))
        })?;
        self.lock_advisory(&mut tx, "bootstrap", "global", false)
            .await?;

        // 1. Create table if not exists
        let create_sql = format!(
            "CREATE TABLE IF NOT EXISTS {t} (
                run_id        TEXT PRIMARY KEY,
                flow_name     TEXT NOT NULL,
                flow          TEXT NOT NULL DEFAULT '',
                status        TEXT NOT NULL,
                started_at    TEXT NOT NULL,
                finished_at   TEXT NOT NULL,
                duration_ms   BIGINT NOT NULL,
                task_results  JSONB NOT NULL DEFAULT '[]',
                agent_count   INTEGER NOT NULL,
                task_count    INTEGER NOT NULL,
                total_tokens  INTEGER DEFAULT 0,
                cached_tokens INTEGER DEFAULT 0,
                tags          JSONB DEFAULT '[]',
                owner_instance_id TEXT NOT NULL DEFAULT '',
                lease_expires_at TEXT NOT NULL DEFAULT '',
                created_at    TIMESTAMPTZ DEFAULT NOW()
            )"
        );
        sqlx::query(sqlx::AssertSqlSafe(create_sql.to_string()))
            .execute(&mut *tx)
            .await
            .map_err(|e| IronCrewError::Validation(format!("Failed to create {t} table: {e}")))?;

        // 2. Add missing columns (heal older schema versions)
        let migrations: &[(&str, &str)] = &[
            (
                "flow",
                &format!("ALTER TABLE {t} ADD COLUMN IF NOT EXISTS flow TEXT NOT NULL DEFAULT ''"),
            ),
            (
                "total_tokens",
                &format!("ALTER TABLE {t} ADD COLUMN IF NOT EXISTS total_tokens INTEGER DEFAULT 0"),
            ),
            (
                "cached_tokens",
                &format!(
                    "ALTER TABLE {t} ADD COLUMN IF NOT EXISTS cached_tokens INTEGER DEFAULT 0"
                ),
            ),
            (
                "tags",
                &format!("ALTER TABLE {t} ADD COLUMN IF NOT EXISTS tags JSONB DEFAULT '[]'"),
            ),
            (
                "created_at",
                &format!(
                    "ALTER TABLE {t} ADD COLUMN IF NOT EXISTS created_at TIMESTAMPTZ DEFAULT NOW()"
                ),
            ),
        ];

        for (col, sql) in migrations {
            sqlx::query(sqlx::AssertSqlSafe(sql.to_string()))
                .execute(&mut *tx)
                .await
                .map_err(|e| {
                    IronCrewError::Validation(format!(
                        "Failed to migrate required run column '{col}': {e}"
                    ))
                })?;
        }

        // Ownership columns are safety-critical: continuing without them
        // would restore global startup abandonment semantics. Fail startup if
        // the database role cannot apply this backward-compatible migration.
        for (column, sql) in [
            (
                "owner_instance_id",
                format!(
                    "ALTER TABLE {t} ADD COLUMN IF NOT EXISTS owner_instance_id TEXT NOT NULL DEFAULT ''"
                ),
            ),
            (
                "lease_expires_at",
                format!(
                    "ALTER TABLE {t} ADD COLUMN IF NOT EXISTS lease_expires_at TEXT NOT NULL DEFAULT ''"
                ),
            ),
        ] {
            sqlx::query(sqlx::AssertSqlSafe(sql))
                .execute(&mut *tx)
                .await
                .map_err(|e| {
                    IronCrewError::Validation(format!(
                        "Failed to migrate required run ownership column '{}': {}",
                        column, e
                    ))
                })?;
        }

        // 3. Heal column types — upgrade TEXT to JSONB if needed
        let type_fixes: &[(&str, &str)] = &[
            ("task_results", &format!(
                "DO $$ BEGIN
                    IF EXISTS (
                        SELECT 1 FROM information_schema.columns
                        WHERE table_name = '{t}' AND column_name = 'task_results' AND data_type = 'text'
                    ) THEN
                        ALTER TABLE {t} ALTER COLUMN task_results TYPE JSONB USING task_results::jsonb;
                        RAISE NOTICE 'Upgraded task_results from TEXT to JSONB';
                    END IF;
                END $$"
            )),
            ("tags", &format!(
                "DO $$ BEGIN
                    IF EXISTS (
                        SELECT 1 FROM information_schema.columns
                        WHERE table_name = '{t}' AND column_name = 'tags' AND data_type = 'text'
                    ) THEN
                        ALTER TABLE {t} ALTER COLUMN tags TYPE JSONB USING tags::jsonb;
                        RAISE NOTICE 'Upgraded tags from TEXT to JSONB';
                    END IF;
                END $$"
            )),
        ];

        for (col, sql) in type_fixes {
            sqlx::query(sqlx::AssertSqlSafe(sql.to_string()))
                .execute(&mut *tx)
                .await
                .map_err(|e| {
                    IronCrewError::Validation(format!(
                        "Failed to migrate run column type '{col}': {e}"
                    ))
                })?;
        }

        // 4. Create indexes (IF NOT EXISTS — safe to run repeatedly)
        let indexes: &[&str] = &[
            &format!("CREATE INDEX IF NOT EXISTS idx_{t}_status ON {t} (status)"),
            &format!("CREATE INDEX IF NOT EXISTS idx_{t}_started_at ON {t} (started_at DESC)"),
            &format!("CREATE INDEX IF NOT EXISTS idx_{t}_flow_name ON {t} (flow_name)"),
            &format!("CREATE INDEX IF NOT EXISTS idx_{t}_tags ON {t} USING GIN (tags)"),
            &format!(
                "CREATE INDEX IF NOT EXISTS idx_{t}_task_results ON {t} USING GIN (task_results)"
            ),
            &format!(
                "CREATE INDEX IF NOT EXISTS idx_{t}_active_lease ON {t} (status, lease_expires_at)"
            ),
        ];

        for sql in indexes {
            sqlx::query(sqlx::AssertSqlSafe(sql.to_string()))
                .execute(&mut *tx)
                .await
                .map_err(|e| {
                    IronCrewError::Validation(format!("Failed to create run index: {e}"))
                })?;
        }

        // 5. Session tables — conversations and dialogs for resumable sessions
        let ct = &self.conversations_table;
        let dt = &self.dialogs_table;

        let session_tables = [
            format!(
                "CREATE TABLE IF NOT EXISTS {ct} (
                    id          TEXT PRIMARY KEY,
                    flow_name   TEXT NOT NULL,
                    agent_name  TEXT NOT NULL,
                    messages    JSONB NOT NULL DEFAULT '[]',
                    created_at  TEXT NOT NULL,
                    updated_at  TEXT NOT NULL,
                    revision    BIGINT NOT NULL DEFAULT 0
                )"
            ),
            format!(
                "CREATE TABLE IF NOT EXISTS {dt} (
                    id          TEXT PRIMARY KEY,
                    flow_name   TEXT NOT NULL,
                    agent_names JSONB NOT NULL DEFAULT '[]',
                    starter     TEXT NOT NULL,
                    transcript  JSONB NOT NULL DEFAULT '[]',
                    next_index  INTEGER NOT NULL,
                    stopped     BOOLEAN NOT NULL DEFAULT FALSE,
                    stop_reason TEXT,
                    created_at  TEXT NOT NULL,
                    updated_at  TEXT NOT NULL,
                    revision    BIGINT NOT NULL DEFAULT 0
                )"
            ),
        ];
        for sql in &session_tables {
            sqlx::query(sqlx::AssertSqlSafe(sql.to_string()))
                .execute(&mut *tx)
                .await
                .map_err(|e| {
                    IronCrewError::Validation(format!("Failed to create session table: {}", e))
                })?;
        }

        // Add flow_path column for schemas predating Phase-1 HITL support.
        // Guarded with IF NOT EXISTS for idempotency (matches the pattern
        // used for total_tokens / cached_tokens / tags above).
        let session_migrations: &[(&str, String)] = &[
            (
                "conversations.flow_path",
                format!("ALTER TABLE {ct} ADD COLUMN IF NOT EXISTS flow_path TEXT"),
            ),
            (
                "dialogs.flow_path",
                format!("ALTER TABLE {dt} ADD COLUMN IF NOT EXISTS flow_path TEXT"),
            ),
            (
                "conversations.revision",
                format!(
                    "ALTER TABLE {ct} ADD COLUMN IF NOT EXISTS revision BIGINT NOT NULL DEFAULT 0"
                ),
            ),
            (
                "dialogs.revision",
                format!(
                    "ALTER TABLE {dt} ADD COLUMN IF NOT EXISTS revision BIGINT NOT NULL DEFAULT 0"
                ),
            ),
        ];
        for (label, sql) in session_migrations {
            sqlx::query(sqlx::AssertSqlSafe(sql.to_string()))
                .execute(&mut *tx)
                .await
                .map_err(|e| {
                    IronCrewError::Validation(format!(
                        "Failed to migrate session column '{label}': {e}"
                    ))
                })?;
        }

        // Enforce the documented `(flow_path, id)` uniqueness for sessions.
        // Earlier versions used `id` as the sole PRIMARY KEY, which meant a
        // save from flow-B would overwrite flow-A's session with the same
        // id. PostgreSQL 15+ is required so we can use `NULLS NOT DISTINCT`
        // and preserve deterministic uniqueness for legacy `flow_path IS NULL`
        // rows as well.
        let session_unique_indexes: &[(&str, String)] = &[
            (
                "conversations: composite unique (flow_path, id)",
                format!(
                    "CREATE UNIQUE INDEX IF NOT EXISTS uniq_{ct}_flow_id \
                     ON {ct} (flow_path, id) NULLS NOT DISTINCT"
                ),
            ),
            (
                "dialogs: composite unique (flow_path, id)",
                format!(
                    "CREATE UNIQUE INDEX IF NOT EXISTS uniq_{dt}_flow_id \
                     ON {dt} (flow_path, id) NULLS NOT DISTINCT"
                ),
            ),
        ];
        for (label, sql) in session_unique_indexes {
            sqlx::query(sqlx::AssertSqlSafe(sql.to_string()))
                .execute(&mut *tx)
                .await
                .map_err(|e| {
                    IronCrewError::Validation(format!(
                        "Failed to enforce session uniqueness '{label}': {e}"
                    ))
                })?;
        }

        // `CREATE INDEX IF NOT EXISTS` accepts a same-named but incompatible
        // index. Verify the exact safety properties before dropping the old
        // primary keys; a mismatch rolls the entire transaction back.
        for (table, index) in [
            (ct.as_str(), format!("uniq_{ct}_flow_id")),
            (dt.as_str(), format!("uniq_{dt}_flow_id")),
        ] {
            let valid: bool = sqlx::query_scalar(
                "SELECT EXISTS (\
                    SELECT 1 \
                    FROM pg_index i \
                    JOIN pg_class idx ON idx.oid = i.indexrelid \
                    JOIN pg_class tbl ON tbl.oid = i.indrelid \
                    JOIN pg_namespace ns ON ns.oid = tbl.relnamespace \
                    WHERE ns.nspname = current_schema() \
                      AND tbl.relname = $1 AND idx.relname = $2 \
                      AND i.indisunique AND i.indnullsnotdistinct \
                      AND i.indnkeyatts = 2 \
                      AND pg_get_indexdef(i.indexrelid, 1, TRUE) = 'flow_path' \
                      AND pg_get_indexdef(i.indexrelid, 2, TRUE) = 'id'\
                )",
            )
            .bind(table)
            .bind(&index)
            .fetch_one(&mut *tx)
            .await
            .map_err(|e| {
                IronCrewError::Validation(format!(
                    "Failed to verify new PostgreSQL session index '{index}': {e}"
                ))
            })?;
            if !valid {
                return Err(IronCrewError::Validation(format!(
                    "PostgreSQL index '{index}' exists without the required UNIQUE NULLS NOT DISTINCT (flow_path, id) properties"
                )));
            }
        }

        for (label, sql) in [
            (
                "conversations: drop legacy id PK",
                format!("ALTER TABLE {ct} DROP CONSTRAINT IF EXISTS {ct}_pkey"),
            ),
            (
                "dialogs: drop legacy id PK",
                format!("ALTER TABLE {dt} DROP CONSTRAINT IF EXISTS {dt}_pkey"),
            ),
        ] {
            sqlx::query(sqlx::AssertSqlSafe(sql))
                .execute(&mut *tx)
                .await
                .map_err(|e| {
                    IronCrewError::Validation(format!(
                        "Failed to remove legacy session constraint '{label}': {e}"
                    ))
                })?;
        }

        // Session indexes — updated_at helps "list recent sessions" queries
        let session_indexes = [
            format!("CREATE INDEX IF NOT EXISTS idx_{ct}_updated_at ON {ct} (updated_at DESC)"),
            format!("CREATE INDEX IF NOT EXISTS idx_{ct}_flow_name ON {ct} (flow_name)"),
            format!("CREATE INDEX IF NOT EXISTS idx_{ct}_flow_path ON {ct} (flow_path)"),
            format!("CREATE INDEX IF NOT EXISTS idx_{dt}_updated_at ON {dt} (updated_at DESC)"),
            format!("CREATE INDEX IF NOT EXISTS idx_{dt}_flow_name ON {dt} (flow_name)"),
            format!("CREATE INDEX IF NOT EXISTS idx_{dt}_flow_path ON {dt} (flow_path)"),
        ];
        for sql in &session_indexes {
            sqlx::query(sqlx::AssertSqlSafe(sql.to_string()))
                .execute(&mut *tx)
                .await
                .map_err(|e| {
                    IronCrewError::Validation(format!("Failed to create session index: {e}"))
                })?;
        }

        // 6. Audit events table
        let at = &self.audit_events_table;
        let audit_sql = format!(
            "CREATE TABLE IF NOT EXISTS {at} (
                id          TEXT PRIMARY KEY,
                timestamp   TEXT NOT NULL,
                action      TEXT NOT NULL,
                flow_path   TEXT,
                target      TEXT,
                actor       TEXT,
                source_ip   TEXT,
                success     BOOLEAN NOT NULL,
                status_code INTEGER NOT NULL,
                metadata    JSONB
            )"
        );
        sqlx::query(sqlx::AssertSqlSafe(audit_sql.to_string()))
            .execute(&mut *tx)
            .await
            .map_err(|e| IronCrewError::Validation(format!("Failed to create {at} table: {e}")))?;

        let audit_indexes: &[String] = &[
            format!("CREATE INDEX IF NOT EXISTS idx_{at}_timestamp_desc ON {at} (timestamp DESC)"),
            format!("CREATE INDEX IF NOT EXISTS idx_{at}_flow_path ON {at} (flow_path)"),
        ];
        for sql in audit_indexes {
            sqlx::query(sqlx::AssertSqlSafe(sql.to_string()))
                .execute(&mut *tx)
                .await
                .map_err(|e| {
                    IronCrewError::Validation(format!("Failed to create audit index: {e}"))
                })?;
        }

        // 7. Durable request idempotency. Keep identifiers derived solely
        // from the validated table prefix; the compact suffixes also keep
        // every index name below PostgreSQL's 63-byte identifier limit at
        // the maximum supported prefix length.
        let it = &self.idempotency_table;
        let idempotency_sql = format!(
            "CREATE TABLE IF NOT EXISTS {it} (
                key_hash            TEXT PRIMARY KEY,
                principal_id        TEXT NOT NULL,
                request_fingerprint TEXT NOT NULL,
                operation           TEXT NOT NULL,
                scope               TEXT NOT NULL,
                resource_id         TEXT NOT NULL,
                exclusive_scope     TEXT,
                attempt_id          TEXT NOT NULL,
                owner_instance_id   TEXT NOT NULL,
                base_revision       BIGINT,
                state               TEXT NOT NULL,
                response_status     INTEGER,
                response_body       TEXT,
                lease_expires_at    TEXT NOT NULL,
                created_at          TEXT NOT NULL,
                updated_at          TEXT NOT NULL,
                completed_at        TEXT,
                expires_at          TEXT,
                cancel_requested_at TEXT,
                ttl_seconds         BIGINT NOT NULL,
                CHECK (state IN ('claimed', 'running', 'completed', 'indeterminate')),
                CHECK (response_status IS NULL OR response_status BETWEEN 100 AND 599),
                CHECK (ttl_seconds > 0)
            )"
        );
        sqlx::query(sqlx::AssertSqlSafe(idempotency_sql))
            .execute(&mut *tx)
            .await
            .map_err(|e| {
                IronCrewError::Validation(format!(
                    "Failed to create PostgreSQL idempotency table '{it}': {e}"
                ))
            })?;

        // Cross-replica run cancellation uses the keyed run ledger as a
        // durable mailbox. This nullable timestamp is intentionally separate
        // from the replay response so existing clients and ledgers remain
        // backwards compatible.
        let add_cancel_requested =
            format!("ALTER TABLE {it} ADD COLUMN IF NOT EXISTS cancel_requested_at TEXT");
        sqlx::query(sqlx::AssertSqlSafe(add_cancel_requested))
            .execute(&mut *tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "Failed to add PostgreSQL idempotent-run cancellation column: {error}"
                ))
            })?;

        // Backfill ledgers created before principal-aware admission. The
        // opaque legacy digest preserves their non-reusability across an
        // upgrade without persisting a bearer credential or raw label.
        let add_principal = format!("ALTER TABLE {it} ADD COLUMN IF NOT EXISTS principal_id TEXT");
        sqlx::query(sqlx::AssertSqlSafe(add_principal))
            .execute(&mut *tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "Failed to add PostgreSQL idempotency principal column: {error}"
                ))
            })?;
        let backfill_principal = format!(
            "UPDATE {it} SET principal_id = $1 \
             WHERE principal_id IS NULL OR principal_id = ''"
        );
        sqlx::query(sqlx::AssertSqlSafe(backfill_principal))
            .bind(PrincipalId::legacy().as_str())
            .execute(&mut *tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "Failed to migrate PostgreSQL idempotency principals: {error}"
                ))
            })?;
        let require_principal = format!("ALTER TABLE {it} ALTER COLUMN principal_id SET NOT NULL");
        sqlx::query(sqlx::AssertSqlSafe(require_principal))
            .execute(&mut *tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "Failed to require PostgreSQL idempotency principals: {error}"
                ))
            })?;
        let principal_constraint = format!("{it}_principal_ck");
        let has_principal_constraint: bool = sqlx::query_scalar(
            "SELECT EXISTS (\
                 SELECT 1 FROM pg_constraint AS con \
                 JOIN pg_class AS tbl ON tbl.oid = con.conrelid \
                 JOIN pg_namespace AS ns ON ns.oid = tbl.relnamespace \
                 WHERE ns.nspname = current_schema() AND tbl.relname = $1 \
                   AND con.conname = $2 AND con.contype = 'c'\
             )",
        )
        .bind(it)
        .bind(&principal_constraint)
        .fetch_one(&mut *tx)
        .await
        .map_err(|error| {
            IronCrewError::Validation(format!(
                "Failed to inspect PostgreSQL idempotency principal constraint: {error}"
            ))
        })?;
        if !has_principal_constraint {
            let sql = format!(
                "ALTER TABLE {it} ADD CONSTRAINT {principal_constraint} \
                 CHECK (length(principal_id) = 64 AND principal_id !~ '[^0-9a-f]')"
            );
            sqlx::query(sqlx::AssertSqlSafe(sql))
                .execute(&mut *tx)
                .await
                .map_err(|error| {
                    IronCrewError::Validation(format!(
                        "Failed to constrain PostgreSQL idempotency principals: {error}"
                    ))
                })?;
        }

        let idempotency_indexes = [
            format!("CREATE INDEX IF NOT EXISTS {it}_exp_idx ON {it} (expires_at)"),
            format!(
                "CREATE INDEX IF NOT EXISTS {it}_res_idx \
                 ON {it} (operation, scope, resource_id)"
            ),
            format!(
                "CREATE UNIQUE INDEX IF NOT EXISTS {it}_scope_uidx \
                 ON {it} (exclusive_scope) \
                 WHERE exclusive_scope IS NOT NULL \
                   AND state IN ('claimed', 'running')"
            ),
        ];
        for sql in &idempotency_indexes {
            sqlx::query(sqlx::AssertSqlSafe(sql.clone()))
                .execute(&mut *tx)
                .await
                .map_err(|e| {
                    IronCrewError::Validation(format!(
                        "Failed to create PostgreSQL idempotency index: {e}"
                    ))
                })?;
        }

        // A compact accounting table avoids COUNT/SUM scans on every claim
        // and completion. The global row is always updated first, then the
        // opaque principal row, matching the application advisory-lock order.
        let accounting = &self.idempotency_accounting_table;
        let accounting_sql = format!(
            "CREATE TABLE IF NOT EXISTS {accounting} (
                principal_id    TEXT PRIMARY KEY,
                is_global       BOOLEAN NOT NULL,
                record_count    BIGINT NOT NULL DEFAULT 0 CHECK (record_count >= 0),
                in_flight_count BIGINT NOT NULL DEFAULT 0 CHECK (in_flight_count >= 0),
                response_bytes  BIGINT NOT NULL DEFAULT 0 CHECK (response_bytes >= 0),
                updated_at      TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
                CHECK ((is_global AND principal_id = 'global') OR
                       (NOT is_global AND length(principal_id) = 64 AND
                        principal_id !~ '[^0-9a-f]'))
            );
            INSERT INTO {accounting} (principal_id, is_global)
            VALUES ('global', TRUE)
            ON CONFLICT (principal_id) DO NOTHING"
        );
        sqlx::raw_sql(sqlx::AssertSqlSafe(accounting_sql))
            .execute(&mut *tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "Failed to create PostgreSQL idempotency accounting table: {error}"
                ))
            })?;

        let accounting_function = format!("{it}_acct_fn");
        let accounting_trigger = format!("{it}_acct_trg");
        let function_sql = format!(
            r#"CREATE OR REPLACE FUNCTION {accounting_function}() RETURNS TRIGGER
               LANGUAGE plpgsql AS $ironcrew$
               DECLARE
                   changed_principal TEXT;
                   record_delta BIGINT := 0;
                   in_flight_delta BIGINT := 0;
                   response_delta BIGINT := 0;
               BEGIN
                   IF TG_OP = 'INSERT' THEN
                       changed_principal := NEW.principal_id;
                       record_delta := 1;
                       in_flight_delta := CASE WHEN NEW.state IN ('claimed', 'running') THEN 1 ELSE 0 END;
                       response_delta := COALESCE(octet_length(NEW.response_body), 0);
                   ELSIF TG_OP = 'DELETE' THEN
                       changed_principal := OLD.principal_id;
                       record_delta := -1;
                       in_flight_delta := -(CASE WHEN OLD.state IN ('claimed', 'running') THEN 1 ELSE 0 END);
                       response_delta := -COALESCE(octet_length(OLD.response_body), 0);
                   ELSE
                       IF OLD.principal_id <> NEW.principal_id THEN
                           RAISE EXCEPTION 'idempotency principal_id is immutable';
                       END IF;
                       changed_principal := NEW.principal_id;
                       in_flight_delta :=
                           (CASE WHEN NEW.state IN ('claimed', 'running') THEN 1 ELSE 0 END) -
                           (CASE WHEN OLD.state IN ('claimed', 'running') THEN 1 ELSE 0 END);
                       response_delta := COALESCE(octet_length(NEW.response_body), 0) -
                                         COALESCE(octet_length(OLD.response_body), 0);
                       IF in_flight_delta = 0 AND response_delta = 0 THEN
                           RETURN NEW;
                       END IF;
                   END IF;

                   UPDATE {accounting}
                   SET record_count = record_count + record_delta,
                       in_flight_count = in_flight_count + in_flight_delta,
                       response_bytes = response_bytes + response_delta,
                       updated_at = clock_timestamp()
                   WHERE principal_id = 'global' AND is_global = TRUE;
                   IF NOT FOUND THEN
                       RAISE EXCEPTION 'global idempotency accounting row is missing';
                   END IF;

                   IF TG_OP = 'INSERT' THEN
                       INSERT INTO {accounting} AS usage
                           (principal_id, is_global, record_count, in_flight_count,
                            response_bytes, updated_at)
                       VALUES (changed_principal, FALSE, record_delta, in_flight_delta,
                               response_delta, clock_timestamp())
                       ON CONFLICT (principal_id) DO UPDATE SET
                           record_count = usage.record_count + EXCLUDED.record_count,
                           in_flight_count = usage.in_flight_count + EXCLUDED.in_flight_count,
                           response_bytes = usage.response_bytes + EXCLUDED.response_bytes,
                           updated_at = clock_timestamp();
                   ELSE
                       UPDATE {accounting}
                       SET record_count = record_count + record_delta,
                           in_flight_count = in_flight_count + in_flight_delta,
                           response_bytes = response_bytes + response_delta,
                           updated_at = clock_timestamp()
                       WHERE principal_id = changed_principal AND is_global = FALSE;
                       IF NOT FOUND THEN
                           RAISE EXCEPTION 'principal idempotency accounting row is missing';
                       END IF;
                   END IF;

                   DELETE FROM {accounting}
                   WHERE principal_id = changed_principal AND is_global = FALSE
                     AND record_count = 0 AND in_flight_count = 0 AND response_bytes = 0;
                   IF TG_OP = 'DELETE' THEN
                       RETURN OLD;
                   END IF;
                   RETURN NEW;
               END;
               $ironcrew$"#
        );
        sqlx::query(sqlx::AssertSqlSafe(function_sql))
            .execute(&mut *tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "Failed to create PostgreSQL idempotency accounting function: {error}"
                ))
            })?;
        let trigger_sql = format!(
            "DROP TRIGGER IF EXISTS {accounting_trigger} ON {it}; \
             CREATE TRIGGER {accounting_trigger} \
             AFTER INSERT OR DELETE OR UPDATE OF principal_id, state, response_body ON {it} \
             FOR EACH ROW EXECUTE FUNCTION {accounting_function}()"
        );
        sqlx::raw_sql(sqlx::AssertSqlSafe(trigger_sql))
            .execute(&mut *tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "Failed to create PostgreSQL idempotency accounting trigger: {error}"
                ))
            })?;

        // Reconcile once during migration. DDL locks on the ledger remain
        // held until commit, so a concurrent write either precedes this scan
        // or runs through the newly installed trigger afterwards.
        let reconcile_accounting = format!(
            "DELETE FROM {accounting} WHERE is_global = FALSE; \
             INSERT INTO {accounting} \
                 (principal_id, is_global, record_count, in_flight_count, response_bytes) \
             SELECT principal_id, FALSE, COUNT(*)::BIGINT, \
                    COUNT(*) FILTER (WHERE state IN ('claimed', 'running'))::BIGINT, \
                    COALESCE(SUM(octet_length(response_body)), 0)::BIGINT \
             FROM {it} GROUP BY principal_id; \
             UPDATE {accounting} SET \
                 record_count = (SELECT COUNT(*)::BIGINT FROM {it}), \
                 in_flight_count = (SELECT COUNT(*)::BIGINT FROM {it} \
                                    WHERE state IN ('claimed', 'running')), \
                 response_bytes = (SELECT COALESCE(SUM(octet_length(response_body)), 0)::BIGINT \
                                   FROM {it}), \
                 updated_at = clock_timestamp() \
             WHERE principal_id = 'global' AND is_global = TRUE"
        );
        sqlx::raw_sql(sqlx::AssertSqlSafe(reconcile_accounting))
            .execute(&mut *tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "Failed to reconcile PostgreSQL idempotency accounting: {error}"
                ))
            })?;

        // 8. Durable human-input mailbox. Question metadata and answers are
        // application-encrypted before they enter SQL; only routing/fencing
        // fields remain queryable. The run FK is both a safety net and the
        // final cleanup path for explicit run deletion.
        let hit = &self.human_inputs_table;
        let human_inputs_sql = format!(
            "CREATE TABLE IF NOT EXISTS {hit} (
                run_id                    TEXT NOT NULL,
                question_id               TEXT NOT NULL,
                flow                      TEXT NOT NULL,
                owner_instance_id         TEXT NOT NULL,
                key_hash                  TEXT NOT NULL,
                attempt_id                TEXT NOT NULL,
                question_digest           TEXT NOT NULL,
                question_key_fingerprint  TEXT NOT NULL,
                question_nonce            BYTEA NOT NULL,
                question_ciphertext       BYTEA NOT NULL,
                answer_key_fingerprint    TEXT,
                answer_nonce              BYTEA,
                answer_ciphertext         BYTEA,
                state                     TEXT NOT NULL DEFAULT 'pending',
                created_at                TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
                expires_at                TIMESTAMPTZ NOT NULL,
                answered_at               TIMESTAMPTZ,
                PRIMARY KEY (run_id, question_id),
                CONSTRAINT {hit}_run_fk FOREIGN KEY (run_id)
                    REFERENCES {t} (run_id) ON DELETE CASCADE,
                CONSTRAINT {hit}_state_ck CHECK (state IN ('pending', 'answered')),
                CONSTRAINT {hit}_payload_ck CHECK (
                    octet_length(question_nonce) > 0 AND
                    octet_length(question_ciphertext) > 0 AND
                    length(question_key_fingerprint) > 0 AND
                    length(question_digest) = 64 AND
                    question_digest !~ '[^0-9a-f]' AND
                    ((state = 'pending' AND answer_key_fingerprint IS NULL AND
                      answer_nonce IS NULL AND answer_ciphertext IS NULL AND
                      answered_at IS NULL) OR
                     (state = 'answered' AND answer_key_fingerprint IS NOT NULL AND
                      answer_nonce IS NOT NULL AND answer_ciphertext IS NOT NULL AND
                      answered_at IS NOT NULL))
                ),
                CONSTRAINT {hit}_expiry_ck CHECK (expires_at > created_at)
            )"
        );
        sqlx::query(sqlx::AssertSqlSafe(human_inputs_sql))
            .execute(&mut *tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "Failed to create PostgreSQL human-input mailbox table '{hit}': {error}"
                ))
            })?;
        // Question AAD gained a semantic digest after the first mailbox
        // rollout. Old rows cannot be authenticated under the new AAD and are
        // intentionally discarded instead of being silently reinterpreted.
        let add_question_digest =
            format!("ALTER TABLE {hit} ADD COLUMN IF NOT EXISTS question_digest TEXT");
        sqlx::query(sqlx::AssertSqlSafe(add_question_digest))
            .execute(&mut *tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "Failed to add PostgreSQL human-input question digest: {error}"
                ))
            })?;
        let discard_legacy_questions = format!("DELETE FROM {hit} WHERE question_digest IS NULL");
        sqlx::query(sqlx::AssertSqlSafe(discard_legacy_questions))
            .execute(&mut *tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "Failed to discard legacy PostgreSQL human-input rows: {error}"
                ))
            })?;
        let require_question_digest =
            format!("ALTER TABLE {hit} ALTER COLUMN question_digest SET NOT NULL");
        sqlx::query(sqlx::AssertSqlSafe(require_question_digest))
            .execute(&mut *tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "Failed to require PostgreSQL human-input question digest: {error}"
                ))
            })?;
        let refresh_human_payload_constraint = format!(
            "ALTER TABLE {hit} DROP CONSTRAINT IF EXISTS {hit}_payload_ck; \
             ALTER TABLE {hit} ADD CONSTRAINT {hit}_payload_ck CHECK (\
                 octet_length(question_nonce) > 0 AND \
                 octet_length(question_ciphertext) > 0 AND \
                 length(question_key_fingerprint) > 0 AND \
                 length(question_digest) = 64 AND \
                 question_digest !~ '[^0-9a-f]' AND \
                 ((state = 'pending' AND answer_key_fingerprint IS NULL AND \
                   answer_nonce IS NULL AND answer_ciphertext IS NULL AND \
                   answered_at IS NULL) OR \
                  (state = 'answered' AND answer_key_fingerprint IS NOT NULL AND \
                   answer_nonce IS NOT NULL AND answer_ciphertext IS NOT NULL AND \
                   answered_at IS NOT NULL))\
             )"
        );
        sqlx::raw_sql(sqlx::AssertSqlSafe(refresh_human_payload_constraint))
            .execute(&mut *tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "Failed to refresh PostgreSQL human-input payload constraint: {error}"
                ))
            })?;
        for sql in [
            format!(
                "CREATE INDEX IF NOT EXISTS {hit}_run_idx ON {hit} (run_id, expires_at) \
                 WHERE state = 'pending'"
            ),
            format!("CREATE INDEX IF NOT EXISTS {hit}_exp_idx ON {hit} (expires_at)"),
        ] {
            sqlx::query(sqlx::AssertSqlSafe(sql))
                .execute(&mut *tx)
                .await
                .map_err(|error| {
                    IronCrewError::Validation(format!(
                        "Failed to create PostgreSQL human-input mailbox index: {error}"
                    ))
                })?;
        }

        // 9. Durable bounded run-event journal. The event table owns payloads,
        // the state table keeps exact per-run replay bounds, and one trigger-
        // maintained singleton accounts global rows/bytes even during
        // cascading run deletion.
        let events = &self.run_events_table;
        let event_state = &self.run_event_state_table;
        let event_usage = &self.run_event_usage_table;
        let run_events_sql = format!(
            "CREATE TABLE IF NOT EXISTS {events} (
                run_id       TEXT NOT NULL,
                sequence     BIGINT NOT NULL,
                event_type   TEXT NOT NULL,
                payload      JSONB NOT NULL,
                payload_bytes BIGINT NOT NULL,
                accounted_bytes BIGINT GENERATED ALWAYS AS (
                    GREATEST(payload_bytes, octet_length(payload::text)::BIGINT,
                             {MIN_ACCOUNTED_RUN_EVENT_BYTES})
                ) STORED,
                created_at   TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
                expires_at   TIMESTAMPTZ NOT NULL,
                PRIMARY KEY (run_id, sequence),
                CONSTRAINT {events}_run_fk FOREIGN KEY (run_id)
                    REFERENCES {t} (run_id) ON DELETE CASCADE,
                CONSTRAINT {events}_payload_ck CHECK (
                    sequence > 0 AND
                    length(event_type) BETWEEN 1 AND 64 AND
                    event_type !~ '[^a-z0-9_]' AND
                    jsonb_typeof(payload) = 'object' AND
                    payload ? 'event' AND payload->>'event' = event_type AND
                    payload_bytes > 0 AND
                    accounted_bytes > 0 AND
                    accounted_bytes <= {HARD_MAX_EVENT_BYTES}
                ),
                CONSTRAINT {events}_expiry_ck CHECK (expires_at > created_at)
            );
            CREATE TABLE IF NOT EXISTS {event_state} (
                run_id                  TEXT PRIMARY KEY,
                flow                    TEXT NOT NULL,
                owner_instance_id       TEXT NOT NULL,
                latest_sequence         BIGINT NOT NULL DEFAULT 0,
                dropped_through         BIGINT NOT NULL DEFAULT 0,
                retained_events         BIGINT NOT NULL DEFAULT 0,
                retained_bytes          BIGINT NOT NULL DEFAULT 0,
                journal_complete        BOOLEAN NOT NULL DEFAULT TRUE,
                eviction_reason         TEXT,
                terminal_event_sequence BIGINT,
                updated_at              TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
                CONSTRAINT {event_state}_run_fk FOREIGN KEY (run_id)
                    REFERENCES {t} (run_id) ON DELETE CASCADE,
                CONSTRAINT {event_state}_bounds_ck CHECK (
                    latest_sequence >= 0 AND dropped_through >= 0 AND
                    dropped_through <= latest_sequence AND
                    retained_events >= 0 AND retained_bytes >= 0 AND
                    ((retained_events = 0 AND retained_bytes = 0) OR
                     (retained_events > 0 AND retained_bytes > 0)) AND
                    (terminal_event_sequence IS NULL OR
                     (terminal_event_sequence > 0 AND
                      terminal_event_sequence <= latest_sequence))
                ),
                CONSTRAINT {event_state}_reason_ck CHECK (
                    (dropped_through = 0 AND eviction_reason IS NULL) OR
                    (dropped_through > 0 AND eviction_reason IN
                        ('writer_backpressure', 'retention', 'global_capacity', 'owner_lost'))
                )
            );
            CREATE TABLE IF NOT EXISTS {event_usage} (
                singleton       BOOLEAN PRIMARY KEY DEFAULT TRUE CHECK (singleton),
                schema_version  INTEGER NOT NULL DEFAULT 0,
                retained_events BIGINT NOT NULL DEFAULT 0,
                retained_bytes  BIGINT NOT NULL DEFAULT 0,
                updated_at      TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
                CONSTRAINT {event_usage}_usage_ck CHECK (
                    schema_version >= 0 AND
                    retained_events >= 0 AND retained_bytes >= 0 AND
                    ((retained_events = 0 AND retained_bytes = 0) OR
                     (retained_events > 0 AND retained_bytes > 0))
                )
            );
            INSERT INTO {event_usage} (singleton) VALUES (TRUE)
            ON CONFLICT (singleton) DO NOTHING"
        );
        sqlx::raw_sql(sqlx::AssertSqlSafe(run_events_sql))
            .execute(&mut *tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "Failed to create PostgreSQL run-event journal tables: {error}"
                ))
            })?;
        let run_event_usage_migration = format!(
            "ALTER TABLE {event_usage} ADD COLUMN IF NOT EXISTS \
                 schema_version INTEGER NOT NULL DEFAULT 0"
        );
        sqlx::query(sqlx::AssertSqlSafe(run_event_usage_migration))
            .execute(&mut *tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "Failed to migrate PostgreSQL run-event schema version: {error}"
                ))
            })?;

        // `payload_bytes` is the storage-neutral compact JSON wire size used
        // for duplicate and page semantics. PostgreSQL's JSONB rendering can
        // be larger, so a generated conservative accounting size prevents a
        // direct/buggy insert from understating memory/storage consumption.
        // Refresh the constraint for databases created by an earlier binary.
        let run_event_payload_accounting_sql = format!(
            "ALTER TABLE {events} ADD COLUMN IF NOT EXISTS accounted_bytes BIGINT \
                 GENERATED ALWAYS AS (\
                     GREATEST(payload_bytes, octet_length(payload::text)::BIGINT, \
                              {MIN_ACCOUNTED_RUN_EVENT_BYTES})\
                 ) STORED; \
             ALTER TABLE {events} DROP CONSTRAINT IF EXISTS {events}_payload_ck; \
             ALTER TABLE {events} ADD CONSTRAINT {events}_payload_ck CHECK (\
                 sequence > 0 AND \
                 length(event_type) BETWEEN 1 AND 64 AND \
                 event_type !~ '[^a-z0-9_]' AND \
                 jsonb_typeof(payload) = 'object' AND \
                 payload ? 'event' AND payload->>'event' = event_type AND \
                 payload_bytes > 0 AND accounted_bytes > 0 AND \
                 accounted_bytes <= {HARD_MAX_EVENT_BYTES}\
             )"
        );
        sqlx::raw_sql(sqlx::AssertSqlSafe(run_event_payload_accounting_sql))
            .execute(&mut *tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "Failed to harden PostgreSQL run-event payload accounting: {error}"
                ))
            })?;

        for sql in [
            format!(
                "CREATE INDEX IF NOT EXISTS {events}_exp_idx ON {events} \
                 (expires_at, run_id, sequence)"
            ),
            format!(
                "CREATE INDEX IF NOT EXISTS {events}_old_idx ON {events} \
                 (created_at, run_id, sequence)"
            ),
        ] {
            sqlx::query(sqlx::AssertSqlSafe(sql))
                .execute(&mut *tx)
                .await
                .map_err(|error| {
                    IronCrewError::Validation(format!(
                        "Failed to create PostgreSQL run-event journal index: {error}"
                    ))
                })?;
        }

        let event_usage_function = format!("{events}_acct_fn");
        let event_usage_trigger = format!("{events}_acct_trg");
        let event_usage_function_sql = format!(
            r#"CREATE OR REPLACE FUNCTION {event_usage_function}() RETURNS TRIGGER
               LANGUAGE plpgsql AS $ironcrew$
               BEGIN
                   IF TG_OP = 'INSERT' THEN
                       UPDATE {event_usage}
                       SET retained_events = retained_events + 1,
                           retained_bytes = retained_bytes + NEW.accounted_bytes,
                           updated_at = clock_timestamp()
                       WHERE singleton = TRUE;
                       IF NOT FOUND THEN
                           RAISE EXCEPTION 'global run-event accounting row is missing';
                       END IF;
                       RETURN NEW;
                   END IF;

                   UPDATE {event_usage}
                   SET retained_events = retained_events - 1,
                       retained_bytes = retained_bytes - OLD.accounted_bytes,
                       updated_at = clock_timestamp()
                   WHERE singleton = TRUE;
                   IF NOT FOUND THEN
                       RAISE EXCEPTION 'global run-event accounting row is missing';
                   END IF;
                   RETURN OLD;
               END;
               $ironcrew$"#
        );
        sqlx::query(sqlx::AssertSqlSafe(event_usage_function_sql))
            .execute(&mut *tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "Failed to create PostgreSQL run-event accounting function: {error}"
                ))
            })?;
        let event_usage_trigger_sql = format!(
            "DROP TRIGGER IF EXISTS {event_usage_trigger} ON {events}; \
             CREATE TRIGGER {event_usage_trigger} \
             AFTER INSERT OR DELETE ON {events} FOR EACH ROW \
             EXECUTE FUNCTION {event_usage_function}()"
        );
        sqlx::raw_sql(sqlx::AssertSqlSafe(event_usage_trigger_sql))
            .execute(&mut *tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "Failed to create PostgreSQL run-event accounting trigger: {error}"
                ))
            })?;

        // Reconcile any journal created by a previous binary before releasing
        // the bootstrap DDL locks. Existing dropped boundaries remain intact;
        // retained counts and global usage are rebuilt from source rows.
        let reconcile_run_events = format!(
            "INSERT INTO {event_state} AS state (
                 run_id, flow, owner_instance_id, latest_sequence,
                 retained_events, retained_bytes, journal_complete,
                 terminal_event_sequence, updated_at
             )
             SELECT event.run_id, run.flow, run.owner_instance_id,
                    MAX(event.sequence), COUNT(*)::BIGINT,
                    SUM(event.accounted_bytes)::BIGINT,
                    MIN(event.sequence) = 1 AND COUNT(*)::BIGINT = MAX(event.sequence),
                    MAX(event.sequence) FILTER (WHERE event.event_type = 'run_complete'),
                    clock_timestamp()
             FROM {events} AS event
             JOIN {t} AS run ON run.run_id = event.run_id
             GROUP BY event.run_id, run.flow, run.owner_instance_id
             ON CONFLICT (run_id) DO UPDATE SET
                 flow = EXCLUDED.flow,
                 owner_instance_id = EXCLUDED.owner_instance_id,
                 latest_sequence = GREATEST(state.latest_sequence, EXCLUDED.latest_sequence),
                 retained_events = EXCLUDED.retained_events,
                 retained_bytes = EXCLUDED.retained_bytes,
                 journal_complete = state.journal_complete AND
                     EXCLUDED.retained_events =
                         EXCLUDED.latest_sequence - state.dropped_through AND
                     (SELECT MIN(retained.sequence) = state.dropped_through + 1
                      FROM {events} AS retained
                      WHERE retained.run_id = state.run_id),
                 terminal_event_sequence = COALESCE(
                     state.terminal_event_sequence, EXCLUDED.terminal_event_sequence),
                 updated_at = clock_timestamp();
             UPDATE {event_usage}
             SET retained_events = (SELECT COUNT(*)::BIGINT FROM {events}),
                 retained_bytes = (SELECT COALESCE(SUM(accounted_bytes), 0)::BIGINT FROM {events}),
                 updated_at = clock_timestamp()
             WHERE singleton = TRUE"
        );
        let stored_run_event_schema_version: i32 =
            sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                "SELECT schema_version FROM {event_usage} \
                 WHERE singleton = TRUE FOR UPDATE"
            )))
            .fetch_one(&mut *tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "Failed to read PostgreSQL run-event schema version: {error}"
                ))
            })?;
        if stored_run_event_schema_version > RUN_EVENT_SCHEMA_VERSION {
            return Err(IronCrewError::Validation(format!(
                "PostgreSQL run-event schema version {stored_run_event_schema_version} is newer than supported version {RUN_EVENT_SCHEMA_VERSION}"
            )));
        }
        if stored_run_event_schema_version < RUN_EVENT_SCHEMA_VERSION {
            sqlx::raw_sql(sqlx::AssertSqlSafe(reconcile_run_events))
                .execute(&mut *tx)
                .await
                .map_err(|error| {
                    IronCrewError::Validation(format!(
                        "Failed to reconcile PostgreSQL run-event journal accounting: {error}"
                    ))
                })?;
            let mark_version_sql = format!(
                "UPDATE {event_usage} SET schema_version = $1, \
                     updated_at = clock_timestamp() WHERE singleton = TRUE"
            );
            sqlx::query(sqlx::AssertSqlSafe(mark_version_sql))
                .bind(RUN_EVENT_SCHEMA_VERSION)
                .execute(&mut *tx)
                .await
                .map_err(|error| {
                    IronCrewError::Validation(format!(
                        "Failed to mark PostgreSQL run-event schema version: {error}"
                    ))
                })?;
        }

        for (column, data_type) in [
            ("owner_instance_id", "text"),
            ("lease_expires_at", "text"),
            ("task_results", "jsonb"),
            ("tags", "jsonb"),
        ] {
            let valid: bool = sqlx::query_scalar(
                "SELECT EXISTS (\
                    SELECT 1 FROM information_schema.columns \
                    WHERE table_schema = current_schema() \
                      AND table_name = $1 AND column_name = $2 AND data_type = $3\
                )",
            )
            .bind(t)
            .bind(column)
            .bind(data_type)
            .fetch_one(&mut *tx)
            .await
            .map_err(|e| {
                IronCrewError::Validation(format!(
                    "Failed to verify required run column '{column}': {e}"
                ))
            })?;
            if !valid {
                return Err(IronCrewError::Validation(format!(
                    "PostgreSQL column '{t}.{column}' is missing or is not {data_type}"
                )));
            }
        }

        tx.commit().await.map_err(|e| {
            IronCrewError::Validation(format!("Failed to commit PostgreSQL bootstrap: {e}"))
        })?;
        self.verify_required_schema().await?;

        tracing::debug!(
            "PostgreSQL bootstrap complete for tables '{}', '{}', '{}', '{}', '{}', '{}', '{}', '{}', '{}', '{}'",
            self.table_name,
            self.conversations_table,
            self.dialogs_table,
            self.audit_events_table,
            self.idempotency_table,
            self.idempotency_accounting_table,
            self.human_inputs_table,
            self.run_events_table,
            self.run_event_state_table,
            self.run_event_usage_table
        );
        Ok(())
    }

    /// Verify invariants required for safe multi-instance operation. Readiness
    /// uses the same check, so a manually altered schema cannot remain ready.
    async fn verify_required_schema(&self) -> Result<()> {
        let required_columns: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM information_schema.columns \
             WHERE table_schema = current_schema() AND table_name = $1 \
               AND (column_name, data_type) IN (\
                   ('owner_instance_id', 'text'), \
                   ('lease_expires_at', 'text'), \
                   ('task_results', 'jsonb'), \
                   ('tags', 'jsonb')\
               )",
        )
        .bind(&self.table_name)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| {
            IronCrewError::Validation(format!("Failed to verify PostgreSQL run schema: {e}"))
        })?;
        if required_columns != 4 {
            return Err(IronCrewError::Validation(format!(
                "PostgreSQL schema for '{}' is missing one or more required typed columns",
                self.table_name
            )));
        }

        let conversation_index = format!("uniq_{}_flow_id", self.conversations_table);
        let dialog_index = format!("uniq_{}_flow_id", self.dialogs_table);
        let valid_indexes: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) \
             FROM pg_index i \
             JOIN pg_class idx ON idx.oid = i.indexrelid \
             JOIN pg_class tbl ON tbl.oid = i.indrelid \
             JOIN pg_namespace ns ON ns.oid = tbl.relnamespace \
             WHERE ns.nspname = current_schema() \
               AND ((tbl.relname = $1 AND idx.relname = $2) \
                 OR (tbl.relname = $3 AND idx.relname = $4)) \
               AND i.indisunique AND i.indnullsnotdistinct \
               AND i.indnkeyatts = 2 \
               AND pg_get_indexdef(i.indexrelid, 1, TRUE) = 'flow_path' \
               AND pg_get_indexdef(i.indexrelid, 2, TRUE) = 'id'",
        )
        .bind(&self.conversations_table)
        .bind(&conversation_index)
        .bind(&self.dialogs_table)
        .bind(&dialog_index)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| {
            IronCrewError::Validation(format!("Failed to verify PostgreSQL session schema: {e}"))
        })?;
        if valid_indexes != 2 {
            return Err(IronCrewError::Validation(
                "PostgreSQL schema is missing a required UNIQUE NULLS NOT DISTINCT (flow_path, id) session index"
                    .into(),
            ));
        }
        let revision_columns: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM information_schema.columns \
             WHERE table_schema = current_schema() \
               AND ((table_name = $1 AND column_name = 'revision' AND data_type = 'bigint') \
                 OR (table_name = $2 AND column_name = 'revision' AND data_type = 'bigint'))",
        )
        .bind(&self.conversations_table)
        .bind(&self.dialogs_table)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| {
            IronCrewError::Validation(format!(
                "Failed to verify PostgreSQL session revisions: {e}"
            ))
        })?;
        if revision_columns != 2 {
            return Err(IronCrewError::Validation(
                "PostgreSQL session tables are missing required BIGINT revision columns".into(),
            ));
        }

        let idempotency_columns: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) \
             FROM information_schema.columns AS c \
             JOIN (VALUES \
                 ('key_hash', 'text'), \
                 ('principal_id', 'text'), \
                 ('request_fingerprint', 'text'), \
                 ('operation', 'text'), \
                 ('scope', 'text'), \
                 ('resource_id', 'text'), \
                 ('exclusive_scope', 'text'), \
                 ('attempt_id', 'text'), \
                 ('owner_instance_id', 'text'), \
                 ('base_revision', 'bigint'), \
                 ('state', 'text'), \
                 ('response_status', 'integer'), \
                 ('response_body', 'text'), \
                 ('lease_expires_at', 'text'), \
                 ('created_at', 'text'), \
                 ('updated_at', 'text'), \
                 ('completed_at', 'text'), \
                 ('expires_at', 'text'), \
                 ('cancel_requested_at', 'text'), \
                 ('ttl_seconds', 'bigint') \
             ) AS required(column_name, data_type) \
               ON required.column_name = c.column_name \
              AND required.data_type = c.data_type \
             WHERE c.table_schema = current_schema() AND c.table_name = $1",
        )
        .bind(&self.idempotency_table)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| {
            IronCrewError::Validation(format!(
                "Failed to verify PostgreSQL idempotency columns: {e}"
            ))
        })?;
        if idempotency_columns != 20 {
            return Err(IronCrewError::Validation(format!(
                "PostgreSQL schema for '{}' is missing one or more required typed columns",
                self.idempotency_table
            )));
        }

        let idempotency_primary_key: bool = sqlx::query_scalar(
            "SELECT EXISTS (\
                 SELECT 1 \
                 FROM pg_constraint con \
                 JOIN pg_class tbl ON tbl.oid = con.conrelid \
                 JOIN pg_namespace ns ON ns.oid = tbl.relnamespace \
                 JOIN pg_attribute attr \
                   ON attr.attrelid = tbl.oid AND attr.attnum = con.conkey[1] \
                 WHERE ns.nspname = current_schema() AND tbl.relname = $1 \
                   AND con.contype = 'p' AND cardinality(con.conkey) = 1 \
                   AND attr.attname = 'key_hash'\
             )",
        )
        .bind(&self.idempotency_table)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| {
            IronCrewError::Validation(format!(
                "Failed to verify PostgreSQL idempotency primary key: {e}"
            ))
        })?;
        if !idempotency_primary_key {
            return Err(IronCrewError::Validation(format!(
                "PostgreSQL table '{}' must have key_hash as its primary key",
                self.idempotency_table
            )));
        }

        let expires_index = format!("{}_exp_idx", self.idempotency_table);
        let resource_index = format!("{}_res_idx", self.idempotency_table);
        let scope_index = format!("{}_scope_uidx", self.idempotency_table);
        let valid_idempotency_indexes: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) \
             FROM pg_index i \
             JOIN pg_class idx ON idx.oid = i.indexrelid \
             JOIN pg_class tbl ON tbl.oid = i.indrelid \
             JOIN pg_namespace ns ON ns.oid = tbl.relnamespace \
             WHERE ns.nspname = current_schema() AND tbl.relname = $1 \
               AND (\
                 (idx.relname = $2 AND i.indnkeyatts = 1 \
                   AND pg_get_indexdef(i.indexrelid, 1, TRUE) = 'expires_at') \
                 OR \
                 (idx.relname = $3 AND i.indnkeyatts = 3 \
                   AND pg_get_indexdef(i.indexrelid, 1, TRUE) = 'operation' \
                   AND pg_get_indexdef(i.indexrelid, 2, TRUE) = 'scope' \
                   AND pg_get_indexdef(i.indexrelid, 3, TRUE) = 'resource_id') \
                 OR \
                 (idx.relname = $4 AND i.indisunique AND i.indnkeyatts = 1 \
                   AND pg_get_indexdef(i.indexrelid, 1, TRUE) = 'exclusive_scope' \
                   AND i.indpred IS NOT NULL \
                   AND pg_get_expr(i.indpred, i.indrelid) LIKE '%exclusive_scope IS NOT NULL%' \
                   AND pg_get_expr(i.indpred, i.indrelid) LIKE '%claimed%' \
                   AND pg_get_expr(i.indpred, i.indrelid) LIKE '%running%')\
               )",
        )
        .bind(&self.idempotency_table)
        .bind(&expires_index)
        .bind(&resource_index)
        .bind(&scope_index)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| {
            IronCrewError::Validation(format!(
                "Failed to verify PostgreSQL idempotency indexes: {e}"
            ))
        })?;
        if valid_idempotency_indexes != 3 {
            return Err(IronCrewError::Validation(format!(
                "PostgreSQL table '{}' is missing one or more required idempotency indexes",
                self.idempotency_table
            )));
        }

        let human_input_columns: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM information_schema.columns AS c \
             JOIN (VALUES \
                 ('run_id', 'text'), \
                 ('question_id', 'text'), \
                 ('flow', 'text'), \
                 ('owner_instance_id', 'text'), \
                 ('key_hash', 'text'), \
                 ('attempt_id', 'text'), \
                 ('question_digest', 'text'), \
                 ('question_key_fingerprint', 'text'), \
                 ('question_nonce', 'bytea'), \
                 ('question_ciphertext', 'bytea'), \
                 ('answer_key_fingerprint', 'text'), \
                 ('answer_nonce', 'bytea'), \
                 ('answer_ciphertext', 'bytea'), \
                 ('state', 'text'), \
                 ('created_at', 'timestamp with time zone'), \
                 ('expires_at', 'timestamp with time zone'), \
                 ('answered_at', 'timestamp with time zone') \
             ) AS required(column_name, data_type) \
               ON required.column_name = c.column_name \
              AND required.data_type = c.data_type \
             WHERE c.table_schema = current_schema() AND c.table_name = $1",
        )
        .bind(&self.human_inputs_table)
        .fetch_one(&self.pool)
        .await
        .map_err(|error| {
            IronCrewError::Validation(format!(
                "Failed to verify PostgreSQL human-input mailbox columns: {error}"
            ))
        })?;
        if human_input_columns != 17 {
            return Err(IronCrewError::Validation(format!(
                "PostgreSQL schema for '{}' is missing one or more human-input mailbox columns",
                self.human_inputs_table
            )));
        }

        let human_input_constraints: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM pg_constraint AS con \
             JOIN pg_class AS tbl ON tbl.oid = con.conrelid \
             JOIN pg_namespace AS ns ON ns.oid = tbl.relnamespace \
             LEFT JOIN pg_class AS referenced ON referenced.oid = con.confrelid \
             LEFT JOIN pg_namespace AS referenced_ns ON referenced_ns.oid = referenced.relnamespace \
             WHERE ns.nspname = current_schema() AND tbl.relname = $1 AND (\
                 (con.contype = 'p' AND cardinality(con.conkey) = 2 AND \
                  (SELECT array_agg(attr.attname ORDER BY key.ordinality) \
                   FROM unnest(con.conkey) WITH ORDINALITY AS key(attnum, ordinality) \
                   JOIN pg_attribute AS attr ON attr.attrelid = tbl.oid \
                                             AND attr.attnum = key.attnum) \
                    = ARRAY['run_id', 'question_id']::name[]) OR \
                 (con.contype = 'f' AND con.confdeltype = 'c' AND \
                  cardinality(con.conkey) = 1 AND \
                  (SELECT attr.attname FROM pg_attribute AS attr \
                   WHERE attr.attrelid = tbl.oid AND attr.attnum = con.conkey[1]) = 'run_id' AND \
                  referenced_ns.nspname = current_schema() AND referenced.relname = $5 AND \
                  (SELECT attr.attname FROM pg_attribute AS attr \
                   WHERE attr.attrelid = referenced.oid AND attr.attnum = con.confkey[1]) = 'run_id') OR \
                 (con.contype = 'c' AND con.conname IN ($2, $3, $4))\
             )",
        )
        .bind(&self.human_inputs_table)
        .bind(format!("{}_state_ck", self.human_inputs_table))
        .bind(format!("{}_payload_ck", self.human_inputs_table))
        .bind(format!("{}_expiry_ck", self.human_inputs_table))
        .bind(&self.table_name)
        .fetch_one(&self.pool)
        .await
        .map_err(|error| {
            IronCrewError::Validation(format!(
                "Failed to verify PostgreSQL human-input mailbox constraints: {error}"
            ))
        })?;
        if human_input_constraints != 5 {
            return Err(IronCrewError::Validation(format!(
                "PostgreSQL table '{}' is missing a required primary key, cascading run foreign key, or state/payload/expiry check",
                self.human_inputs_table
            )));
        }

        let human_run_index = format!("{}_run_idx", self.human_inputs_table);
        let human_expiry_index = format!("{}_exp_idx", self.human_inputs_table);
        let human_input_indexes: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM pg_index AS i \
             JOIN pg_class AS idx ON idx.oid = i.indexrelid \
             JOIN pg_class AS tbl ON tbl.oid = i.indrelid \
             JOIN pg_namespace AS ns ON ns.oid = tbl.relnamespace \
             WHERE ns.nspname = current_schema() AND tbl.relname = $1 AND (\
                 (idx.relname = $2 AND i.indnkeyatts = 2 AND \
                  pg_get_indexdef(i.indexrelid, 1, TRUE) = 'run_id' AND \
                  pg_get_indexdef(i.indexrelid, 2, TRUE) = 'expires_at' AND \
                  i.indpred IS NOT NULL AND \
                  pg_get_expr(i.indpred, i.indrelid) LIKE '%pending%') OR \
                 (idx.relname = $3 AND i.indnkeyatts = 1 AND \
                  pg_get_indexdef(i.indexrelid, 1, TRUE) = 'expires_at')\
             )",
        )
        .bind(&self.human_inputs_table)
        .bind(&human_run_index)
        .bind(&human_expiry_index)
        .fetch_one(&self.pool)
        .await
        .map_err(|error| {
            IronCrewError::Validation(format!(
                "Failed to verify PostgreSQL human-input mailbox indexes: {error}"
            ))
        })?;
        if human_input_indexes != 2 {
            return Err(IronCrewError::Validation(format!(
                "PostgreSQL table '{}' is missing one or more required human-input mailbox indexes",
                self.human_inputs_table
            )));
        }

        let accounting_columns: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM information_schema.columns AS c \
             JOIN (VALUES \
                 ('principal_id', 'text'), \
                 ('is_global', 'boolean'), \
                 ('record_count', 'bigint'), \
                 ('in_flight_count', 'bigint'), \
                 ('response_bytes', 'bigint'), \
                 ('updated_at', 'timestamp with time zone') \
             ) AS required(column_name, data_type) \
               ON required.column_name = c.column_name \
              AND required.data_type = c.data_type \
             WHERE c.table_schema = current_schema() AND c.table_name = $1",
        )
        .bind(&self.idempotency_accounting_table)
        .fetch_one(&self.pool)
        .await
        .map_err(|error| {
            IronCrewError::Validation(format!(
                "Failed to verify PostgreSQL idempotency accounting columns: {error}"
            ))
        })?;
        if accounting_columns != 6 {
            return Err(IronCrewError::Validation(format!(
                "PostgreSQL schema for '{}' is missing one or more accounting columns",
                self.idempotency_accounting_table
            )));
        }
        let accounting_trigger = format!("{}_acct_trg", self.idempotency_table);
        let trigger_exists: bool = sqlx::query_scalar(
            "SELECT EXISTS (\
                 SELECT 1 FROM pg_trigger AS trg \
                 JOIN pg_class AS tbl ON tbl.oid = trg.tgrelid \
                 JOIN pg_namespace AS ns ON ns.oid = tbl.relnamespace \
                 WHERE ns.nspname = current_schema() AND tbl.relname = $1 \
                   AND trg.tgname = $2 AND NOT trg.tgisinternal \
                   AND trg.tgenabled <> 'D'\
             )",
        )
        .bind(&self.idempotency_table)
        .bind(&accounting_trigger)
        .fetch_one(&self.pool)
        .await
        .map_err(|error| {
            IronCrewError::Validation(format!(
                "Failed to verify PostgreSQL idempotency accounting trigger: {error}"
            ))
        })?;
        if !trigger_exists {
            return Err(IronCrewError::Validation(
                "PostgreSQL idempotency accounting trigger is missing or disabled".into(),
            ));
        }
        let global_accounting_valid: bool = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT EXISTS (SELECT 1 FROM {} \
                 WHERE principal_id = 'global' AND is_global = TRUE \
                   AND record_count >= 0 AND in_flight_count >= 0 AND response_bytes >= 0)",
            self.idempotency_accounting_table
        )))
        .fetch_one(&self.pool)
        .await
        .map_err(|error| {
            IronCrewError::Validation(format!(
                "Failed to verify PostgreSQL global idempotency accounting: {error}"
            ))
        })?;
        if !global_accounting_valid {
            return Err(IronCrewError::Validation(
                "PostgreSQL global idempotency accounting row is missing or invalid".into(),
            ));
        }

        let run_event_columns: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM information_schema.columns AS column_info \
             JOIN (VALUES \
                 ($1::text, 'run_id', 'text'), \
                 ($1::text, 'sequence', 'bigint'), \
                 ($1::text, 'event_type', 'text'), \
                 ($1::text, 'payload', 'jsonb'), \
                 ($1::text, 'payload_bytes', 'bigint'), \
                 ($1::text, 'accounted_bytes', 'bigint'), \
                 ($1::text, 'created_at', 'timestamp with time zone'), \
                 ($1::text, 'expires_at', 'timestamp with time zone'), \
                 ($2::text, 'run_id', 'text'), \
                 ($2::text, 'flow', 'text'), \
                 ($2::text, 'owner_instance_id', 'text'), \
                 ($2::text, 'latest_sequence', 'bigint'), \
                 ($2::text, 'dropped_through', 'bigint'), \
                 ($2::text, 'retained_events', 'bigint'), \
                 ($2::text, 'retained_bytes', 'bigint'), \
                 ($2::text, 'journal_complete', 'boolean'), \
                 ($2::text, 'eviction_reason', 'text'), \
                 ($2::text, 'terminal_event_sequence', 'bigint'), \
                 ($2::text, 'updated_at', 'timestamp with time zone'), \
                 ($3::text, 'singleton', 'boolean'), \
                 ($3::text, 'schema_version', 'integer'), \
                 ($3::text, 'retained_events', 'bigint'), \
                 ($3::text, 'retained_bytes', 'bigint'), \
                 ($3::text, 'updated_at', 'timestamp with time zone') \
             ) AS required(table_name, column_name, data_type) \
               ON required.table_name = column_info.table_name \
              AND required.column_name = column_info.column_name \
              AND required.data_type = column_info.data_type \
             WHERE column_info.table_schema = current_schema()",
        )
        .bind(&self.run_events_table)
        .bind(&self.run_event_state_table)
        .bind(&self.run_event_usage_table)
        .fetch_one(&self.pool)
        .await
        .map_err(|error| {
            IronCrewError::Validation(format!(
                "Failed to verify PostgreSQL run-event journal columns: {error}"
            ))
        })?;
        if run_event_columns != 24 {
            return Err(IronCrewError::Validation(
                "PostgreSQL run-event journal is missing one or more required typed columns".into(),
            ));
        }

        let run_event_constraints: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM pg_constraint AS con \
             JOIN pg_class AS table_info ON table_info.oid = con.conrelid \
             JOIN pg_namespace AS namespace ON namespace.oid = table_info.relnamespace \
             LEFT JOIN pg_class AS referenced ON referenced.oid = con.confrelid \
             LEFT JOIN pg_namespace AS referenced_namespace \
                    ON referenced_namespace.oid = referenced.relnamespace \
             WHERE namespace.nspname = current_schema() AND (\
                 (table_info.relname = $1 AND con.contype = 'p' AND \
                  cardinality(con.conkey) = 2) OR \
                 (table_info.relname = $2 AND con.contype = 'p' AND \
                  cardinality(con.conkey) = 1) OR \
                 (table_info.relname = $3 AND con.contype = 'p' AND \
                  cardinality(con.conkey) = 1) OR \
                 (table_info.relname IN ($1, $2) AND con.contype = 'f' AND \
                  con.confdeltype = 'c' AND referenced_namespace.nspname = current_schema() AND \
                  referenced.relname = $4) OR \
                 (con.contype = 'c' AND con.conname IN ($5, $6, $7, $8, $9))\
             )",
        )
        .bind(&self.run_events_table)
        .bind(&self.run_event_state_table)
        .bind(&self.run_event_usage_table)
        .bind(&self.table_name)
        .bind(format!("{}_payload_ck", self.run_events_table))
        .bind(format!("{}_expiry_ck", self.run_events_table))
        .bind(format!("{}_bounds_ck", self.run_event_state_table))
        .bind(format!("{}_reason_ck", self.run_event_state_table))
        .bind(format!("{}_usage_ck", self.run_event_usage_table))
        .fetch_one(&self.pool)
        .await
        .map_err(|error| {
            IronCrewError::Validation(format!(
                "Failed to verify PostgreSQL run-event journal constraints: {error}"
            ))
        })?;
        if run_event_constraints != 10 {
            return Err(IronCrewError::Validation(
                "PostgreSQL run-event journal is missing a primary key, cascading run foreign key, or bounded-data constraint"
                    .into(),
            ));
        }

        let run_event_expiry_index = format!("{}_exp_idx", self.run_events_table);
        let run_event_oldest_index = format!("{}_old_idx", self.run_events_table);
        let run_event_indexes: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM pg_index AS index_info \
             JOIN pg_class AS index_class ON index_class.oid = index_info.indexrelid \
             JOIN pg_class AS table_info ON table_info.oid = index_info.indrelid \
             JOIN pg_namespace AS namespace ON namespace.oid = table_info.relnamespace \
             WHERE namespace.nspname = current_schema() AND table_info.relname = $1 AND (\
                 (index_class.relname = $2 AND index_info.indnkeyatts = 3 AND \
                  pg_get_indexdef(index_info.indexrelid, 1, TRUE) = 'expires_at' AND \
                  pg_get_indexdef(index_info.indexrelid, 2, TRUE) = 'run_id' AND \
                  pg_get_indexdef(index_info.indexrelid, 3, TRUE) = 'sequence') OR \
                 (index_class.relname = $3 AND index_info.indnkeyatts = 3 AND \
                  pg_get_indexdef(index_info.indexrelid, 1, TRUE) = 'created_at' AND \
                  pg_get_indexdef(index_info.indexrelid, 2, TRUE) = 'run_id' AND \
                  pg_get_indexdef(index_info.indexrelid, 3, TRUE) = 'sequence')\
             )",
        )
        .bind(&self.run_events_table)
        .bind(&run_event_expiry_index)
        .bind(&run_event_oldest_index)
        .fetch_one(&self.pool)
        .await
        .map_err(|error| {
            IronCrewError::Validation(format!(
                "Failed to verify PostgreSQL run-event journal indexes: {error}"
            ))
        })?;
        if run_event_indexes != 2 {
            return Err(IronCrewError::Validation(
                "PostgreSQL run-event journal is missing a retention or global-pruning index"
                    .into(),
            ));
        }

        let run_event_trigger = format!("{}_acct_trg", self.run_events_table);
        let run_event_trigger_exists: bool = sqlx::query_scalar(
            "SELECT EXISTS (\
                 SELECT 1 FROM pg_trigger AS trigger_info \
                 JOIN pg_class AS table_info ON table_info.oid = trigger_info.tgrelid \
                 JOIN pg_namespace AS namespace ON namespace.oid = table_info.relnamespace \
                 WHERE namespace.nspname = current_schema() AND table_info.relname = $1 \
                   AND trigger_info.tgname = $2 AND NOT trigger_info.tgisinternal \
                   AND trigger_info.tgenabled <> 'D'\
             )",
        )
        .bind(&self.run_events_table)
        .bind(&run_event_trigger)
        .fetch_one(&self.pool)
        .await
        .map_err(|error| {
            IronCrewError::Validation(format!(
                "Failed to verify PostgreSQL run-event accounting trigger: {error}"
            ))
        })?;
        if !run_event_trigger_exists {
            return Err(IronCrewError::Validation(
                "PostgreSQL run-event accounting trigger is missing or disabled".into(),
            ));
        }

        let run_event_usage_valid: bool = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT EXISTS (SELECT 1 FROM {} WHERE singleton = TRUE \
                     AND schema_version = {RUN_EVENT_SCHEMA_VERSION} \
                     AND retained_events >= 0 AND retained_bytes >= 0)",
            self.run_event_usage_table
        )))
        .fetch_one(&self.pool)
        .await
        .map_err(|error| {
            IronCrewError::Validation(format!(
                "Failed to verify PostgreSQL run-event global accounting: {error}"
            ))
        })?;
        if !run_event_usage_valid {
            return Err(IronCrewError::Validation(
                "PostgreSQL run-event global accounting row is missing or invalid".into(),
            ));
        }
        Ok(())
    }
}

async fn ensure_supported_postgres_version(pool: &PgPool) -> Result<()> {
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

#[async_trait]
impl StateStore for PostgresStore {
    async fn save_run_intent(&self, intent: RunIntent) -> Result<String> {
        let run_id = intent
            .suggested_id
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let tags_json = serde_json::to_string(&intent.tags)
            .map_err(|e| IronCrewError::Validation(format!("Tags serialize: {}", e)))?;
        let empty_tasks = serde_json::to_string(&serde_json::Value::Array(Vec::new()))
            .map_err(|e| IronCrewError::Validation(format!("Empty tasks serialize: {}", e)))?;
        let sql = format!(
            "INSERT INTO {} (run_id, flow_name, flow, status, started_at, finished_at, duration_ms, task_results, agent_count, task_count, total_tokens, cached_tokens, tags, owner_instance_id, lease_expires_at)
             VALUES ($1, $2, $3, 'running', $4, '', 0, $5::jsonb, $6, $7, 0, 0, $8::jsonb, $9, $10)
             ON CONFLICT (run_id) DO NOTHING",
            self.table_name
        );
        let mut tx = self.pool.begin().await.map_err(|error| {
            IronCrewError::Validation(format!("PG insert intent transaction: {error}"))
        })?;
        self.lock_run_fence(&mut tx, true).await?;
        self.lock_resource(&mut tx, RUN_OPERATION, "", &run_id)
            .await?;
        let (database_now, lease_expires_at) = self
            .database_clock_with_deadline(&mut tx, self.lease.ttl().as_secs(), "run intent lease")
            .await?;
        let inserted = sqlx::query(sqlx::AssertSqlSafe(sql.to_string()))
            .bind(&run_id)
            .bind(&intent.flow_name)
            .bind(&intent.flow)
            .bind(&intent.started_at)
            .bind(&empty_tasks)
            .bind(intent.agent_count as i64)
            .bind(intent.task_count as i64)
            .bind(&tags_json)
            .bind(self.lease.instance_id())
            .bind(&lease_expires_at)
            .execute(&mut *tx)
            .await
            .map_err(|e| IronCrewError::Validation(format!("PG insert intent: {}", e)))?;
        if inserted.rows_affected() == 0 {
            let hydrate_sql = format!(
                "UPDATE {runs} AS run SET \
                     flow_name = $1, agent_count = $2, task_count = $3, \
                     tags = $4::jsonb, lease_expires_at = $5 \
                 WHERE run.run_id = $6 AND run.flow = $7 \
                   AND run.owner_instance_id = $8 \
                   AND run.status IN ('running', 'waiting_for_input') \
                   AND EXISTS (\
                       SELECT 1 FROM {idempotency} AS idem \
                       WHERE idem.operation = $9 AND idem.scope = $7 \
                         AND idem.resource_id = $6 \
                         AND idem.owner_instance_id = $8 \
                         AND idem.state IN ('running', 'completed')\
                   )",
                runs = self.table_name,
                idempotency = self.idempotency_table
            );
            let hydrated = sqlx::query(sqlx::AssertSqlSafe(hydrate_sql))
                .bind(&intent.flow_name)
                .bind(intent.agent_count as i64)
                .bind(intent.task_count as i64)
                .bind(&tags_json)
                .bind(&lease_expires_at)
                .bind(&run_id)
                .bind(&intent.flow)
                .bind(self.lease.instance_id())
                .bind(RUN_OPERATION)
                .execute(&mut *tx)
                .await
                .map_err(|error| {
                    IronCrewError::Validation(format!(
                        "PG idempotent provisional run hydration: {error}"
                    ))
                })?;
            if hydrated.rows_affected() != 1 {
                return Err(IronCrewError::Conflict(format!(
                    "Run '{run_id}' already exists without a matching idempotent provisional intent"
                )));
            }
        }
        let mapping_sql = format!(
            "UPDATE {} SET state = 'running', lease_expires_at = $1, updated_at = $2 \
             WHERE operation = $3 AND scope = $4 AND resource_id = $5 \
               AND owner_instance_id = $6 AND state = 'claimed'",
            self.idempotency_table
        );
        sqlx::query(sqlx::AssertSqlSafe(mapping_sql))
            .bind(&lease_expires_at)
            .bind(&database_now)
            .bind(RUN_OPERATION)
            .bind(&intent.flow)
            .bind(&run_id)
            .bind(self.lease.instance_id())
            .execute(&mut *tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!("PG run idempotency mapping transition: {error}"))
            })?;
        tx.commit().await.map_err(|error| {
            IronCrewError::Validation(format!("PG insert intent commit: {error}"))
        })?;
        tracing::debug!("Run intent saved: {}", run_id);
        Ok(run_id)
    }

    async fn update_run_completion(
        &self,
        run_id: &str,
        completion: RunCompletion,
    ) -> Result<RunTransition> {
        completion.validate()?;
        let task_results_json = serde_json::to_string(&completion.task_results)
            .map_err(|e| IronCrewError::Validation(format!("task_results serialize: {}", e)))?;
        let mut tx = self.pool.begin().await.map_err(|error| {
            IronCrewError::Validation(format!("PG update completion transaction: {error}"))
        })?;
        self.lock_idempotency_quota(&mut tx).await?;
        self.lock_run_fence(&mut tx, true).await?;
        self.lock_resource(&mut tx, RUN_OPERATION, "", run_id)
            .await?;
        let (database_now, _) = self
            .database_clock_with_deadline(&mut tx, 0, "run completion")
            .await?;
        let sql = format!(
            "UPDATE {}
             SET status = $1, finished_at = $2, duration_ms = $3,
                 task_results = $4::jsonb, total_tokens = $5, cached_tokens = $6,
                 lease_expires_at = ''
             WHERE run_id = $7 AND status IN ('running', 'waiting_for_input')
               AND owner_instance_id = $8",
            self.table_name
        );
        let result = sqlx::query(sqlx::AssertSqlSafe(sql.to_string()))
            .bind(completion.status.to_string())
            .bind(&completion.finished_at)
            .bind(completion.duration_ms as i64)
            .bind(&task_results_json)
            .bind(completion.total_tokens as i32)
            .bind(completion.cached_tokens as i32)
            .bind(run_id)
            .bind(self.lease.instance_id())
            .execute(&mut *tx)
            .await
            .map_err(|e| IronCrewError::Validation(format!("PG update completion: {}", e)))?;

        let transition = if result.rows_affected() == 0 {
            let sql = format!(
                "SELECT status, owner_instance_id, finished_at FROM {} WHERE run_id = $1 FOR UPDATE",
                self.table_name
            );
            let row = sqlx::query(sqlx::AssertSqlSafe(sql.to_string()))
                .bind(run_id)
                .fetch_optional(&mut *tx)
                .await
                .map_err(|e| {
                    IronCrewError::Validation(format!("PG completion state query: {}", e))
                })?;
            let Some(row) = row else {
                return Err(IronCrewError::Validation(format!(
                    "Run '{}' not found",
                    run_id
                )));
            };
            let status: String = row
                .try_get("status")
                .map_err(|e| IronCrewError::Validation(format!("Column error: {}", e)))?;
            let parsed = status.parse::<RunStatus>()?;
            if parsed.is_terminal() {
                RunTransition::AlreadyTerminal(parsed)
            } else {
                let owner: String = row
                    .try_get("owner_instance_id")
                    .map_err(|e| IronCrewError::Validation(format!("Column error: {}", e)))?;
                return Err(IronCrewError::Validation(format!(
                    "Run '{}' is owned by instance '{}', not '{}'",
                    run_id,
                    owner,
                    self.lease.instance_id()
                )));
            }
        } else {
            RunTransition::Applied
        };

        let mapping_sql = format!(
            "UPDATE {} SET state = 'completed', lease_expires_at = '', \
             updated_at = $1, completed_at = $1, \
             expires_at = to_char(\
                 ($1::timestamptz + ttl_seconds * interval '1 second') AT TIME ZONE 'UTC', \
                 'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"'\
             ) \
             WHERE operation = $2 AND resource_id = $3 \
               AND state IN ('claimed', 'running', 'indeterminate')",
            self.idempotency_table
        );
        sqlx::query(sqlx::AssertSqlSafe(mapping_sql))
            .bind(&database_now)
            .bind(RUN_OPERATION)
            .bind(run_id)
            .execute(&mut *tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PG run idempotency completion transition: {error}"
                ))
            })?;
        self.delete_human_inputs_for_run(&mut tx, run_id).await?;
        tx.commit().await.map_err(|error| {
            IronCrewError::Validation(format!("PG update completion commit: {error}"))
        })?;
        tracing::info!("Run completion saved: {} ({})", run_id, completion.status);
        Ok(transition)
    }

    async fn update_run_status(
        &self,
        run_id: &str,
        status: crate::engine::run_history::RunStatus,
    ) -> Result<()> {
        if !status.is_in_flight() {
            return Err(IronCrewError::Validation(format!(
                "update_run_status requires an in-flight status, got '{}'",
                status
            )));
        }
        let sql = format!(
            "UPDATE {} SET status = $1
             WHERE run_id = $2 AND status IN ('running', 'waiting_for_input')
               AND owner_instance_id = $3",
            self.table_name
        );
        let result = sqlx::query(sqlx::AssertSqlSafe(sql.to_string()))
            .bind(status.to_string())
            .bind(run_id)
            .bind(self.lease.instance_id())
            .execute(&self.pool)
            .await
            .map_err(|e| IronCrewError::Validation(format!("PG update status: {}", e)))?;
        if result.rows_affected() == 0 {
            return Err(IronCrewError::Validation(format!(
                "Run '{}' not found or not in an in-flight state",
                run_id
            )));
        }
        Ok(())
    }

    fn instance_id(&self) -> &str {
        self.lease.instance_id()
    }

    fn run_lease_ttl(&self) -> Duration {
        self.lease.ttl()
    }

    fn supports_durable_human_input(&self) -> bool {
        self.human_input_keyring.is_some()
    }

    fn event_journal_scope(&self) -> EventJournalScope {
        EventJournalScope::SharedStore
    }

    fn event_journal_config(&self) -> RunEventJournalConfig {
        self.run_event_journal_config.clone()
    }

    async fn append_run_events(
        &self,
        batch: &RunEventAppendBatch,
    ) -> Result<RunEventAppendOutcome> {
        batch.validate(&self.run_event_journal_config)?;
        if batch.owner_instance_id != self.lease.instance_id() {
            return Err(IronCrewError::Conflict(format!(
                "Run-event batch owner '{}' does not match this store instance",
                batch.owner_instance_id
            )));
        }
        let sequence_values: Vec<i64> = batch
            .entries
            .iter()
            .map(|entry| {
                i64::try_from(entry.sequence).map_err(|_| {
                    IronCrewError::Validation("Run-event sequence exceeds PostgreSQL BIGINT".into())
                })
            })
            .collect::<Result<_>>()?;

        let mut tx = self.pool.begin().await.map_err(|error| {
            IronCrewError::Validation(format!(
                "PostgreSQL run-event append transaction failed: {error}"
            ))
        })?;
        self.lock_run_event_usage(&mut tx).await?;
        let run_sql = format!(
            "SELECT flow, owner_instance_id, status, \
                    CASE WHEN lease_expires_at = '' THEN FALSE ELSE \
                        lease_expires_at::timestamptz > clock_timestamp() \
                    END AS lease_active, \
                    duration_ms, total_tokens \
             FROM {} WHERE run_id = $1 FOR UPDATE",
            self.table_name
        );
        let run: Option<(String, String, String, bool, i64, i32)> =
            sqlx::query_as(sqlx::AssertSqlSafe(run_sql))
                .bind(&batch.run_id)
                .fetch_optional(&mut *tx)
                .await
                .map_err(|error| {
                    IronCrewError::Validation(format!(
                        "PostgreSQL run-event run fence lookup failed: {error}"
                    ))
                })?;
        let (run_flow, run_owner, run_status, lease_active, duration_ms, total_tokens) = run
            .ok_or_else(|| {
                IronCrewError::Validation(format!("Run '{}' not found", batch.run_id))
            })?;
        if run_flow != batch.flow {
            return Err(IronCrewError::Conflict(format!(
                "Run-event flow '{}' does not match run '{}'",
                batch.flow, batch.run_id
            )));
        }
        if run_owner != batch.owner_instance_id {
            return Err(IronCrewError::Conflict(format!(
                "Run '{}' is owned by instance '{}', not '{}'",
                batch.run_id, run_owner, batch.owner_instance_id
            )));
        }

        let mut pruned = self.prune_expired_run_events(&mut tx).await?;
        let initialize_state_sql = format!(
            "INSERT INTO {} (run_id, flow, owner_instance_id) \
             VALUES ($1, $2, $3) ON CONFLICT (run_id) DO NOTHING",
            self.run_event_state_table
        );
        sqlx::query(sqlx::AssertSqlSafe(initialize_state_sql))
            .bind(&batch.run_id)
            .bind(&batch.flow)
            .bind(&batch.owner_instance_id)
            .execute(&mut *tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL run-event state initialization failed: {error}"
                ))
            })?;
        let mut state = self
            .run_event_state_for_update(&mut tx, &batch.run_id)
            .await?
            .ok_or_else(|| {
                IronCrewError::Validation("PostgreSQL run-event state is missing".into())
            })?;
        if state.flow != batch.flow || state.owner_instance_id != batch.owner_instance_id {
            return Err(IronCrewError::Conflict(format!(
                "Run-event state fence for '{}' does not match the current run owner/flow",
                batch.run_id
            )));
        }

        let existing_sql = format!(
            "SELECT sequence, event_type, payload::text AS payload, payload_bytes \
             FROM {} WHERE run_id = $1 AND sequence = ANY($2::bigint[])",
            self.run_events_table
        );
        let existing_rows = sqlx::query(sqlx::AssertSqlSafe(existing_sql))
            .bind(&batch.run_id)
            .bind(&sequence_values)
            .fetch_all(&mut *tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL run-event duplicate lookup failed: {error}"
                ))
            })?;
        let mut existing = HashMap::with_capacity(existing_rows.len());
        for row in existing_rows {
            let sequence = nonnegative_u64(
                "stored sequence",
                row.try_get("sequence").map_err(|error| {
                    IronCrewError::Validation(format!("Run-event sequence column: {error}"))
                })?,
            )?;
            let event_type: String = row.try_get("event_type").map_err(|error| {
                IronCrewError::Validation(format!("Run-event event_type column: {error}"))
            })?;
            let payload_raw: String = row.try_get("payload").map_err(|error| {
                IronCrewError::Validation(format!("Run-event payload column: {error}"))
            })?;
            let payload: serde_json::Value =
                decode_stored_json(&payload_raw, "run_events.payload")?;
            let payload_bytes = nonnegative_u64(
                "stored payload byte count",
                row.try_get("payload_bytes").map_err(|error| {
                    IronCrewError::Validation(format!("Run-event payload_bytes column: {error}"))
                })?,
            )?;
            existing.insert(sequence, (event_type, payload, payload_bytes));
        }

        let mut duplicate_events = 0u64;
        let mut new_entries = Vec::new();
        for entry in &batch.entries {
            match existing.get(&entry.sequence) {
                Some((event_type, payload, payload_bytes)) => {
                    if entry.sequence > state.latest_sequence {
                        return Err(IronCrewError::Conflict(format!(
                            "Run-event sequence {} for '{}' exists beyond the journal state boundary",
                            entry.sequence, batch.run_id
                        )));
                    }
                    let expected_bytes = u64::try_from(entry.payload_bytes).map_err(|_| {
                        IronCrewError::Validation(
                            "Run-event payload byte count exceeds BIGINT".into(),
                        )
                    })?;
                    if event_type != &entry.event_type
                        || payload != &entry.payload
                        || *payload_bytes != expected_bytes
                    {
                        return Err(IronCrewError::Conflict(format!(
                            "Run-event sequence {} for '{}' already contains different data",
                            entry.sequence, batch.run_id
                        )));
                    }
                    duplicate_events = duplicate_events.saturating_add(1);
                }
                None if entry.sequence <= state.latest_sequence => {
                    return Err(IronCrewError::Conflict(format!(
                        "Run-event sequence {} for '{}' was already allocated and is no longer retained",
                        entry.sequence, batch.run_id
                    )));
                }
                None => new_entries.push(entry),
            }
        }

        let run_status = run_status.parse::<RunStatus>()?;
        if !new_entries.is_empty() {
            if state.terminal_event_sequence.is_some() {
                return Err(IronCrewError::Conflict(format!(
                    "Run-event journal for '{}' is sealed after run_complete",
                    batch.run_id
                )));
            }
            if run_status.is_in_flight() {
                if !lease_active {
                    return Err(IronCrewError::Conflict(format!(
                        "Run '{}' no longer has an active owner lease",
                        batch.run_id
                    )));
                }
                if new_entries
                    .iter()
                    .any(|entry| entry.event_type == "run_complete")
                {
                    return Err(IronCrewError::Conflict(format!(
                        "Run '{}' cannot append run_complete before its terminal record",
                        batch.run_id
                    )));
                }
            } else {
                if matches!(&run_status, RunStatus::Abandoned) {
                    return Err(IronCrewError::Conflict(format!(
                        "Abandoned run '{}' cannot append terminal journal events",
                        batch.run_id
                    )));
                }
                let [terminal_entry] = new_entries.as_slice() else {
                    return Err(IronCrewError::Conflict(format!(
                        "Terminal run '{}' accepts exactly one new run_complete event",
                        batch.run_id
                    )));
                };
                if terminal_entry.event_type != "run_complete" {
                    return Err(IronCrewError::Conflict(format!(
                        "Terminal run '{}' cannot append a nonterminal journal event",
                        batch.run_id
                    )));
                }
                let expected_duration_ms = nonnegative_u64("terminal duration", duration_ms)?;
                let expected_total_tokens = u32::try_from(total_tokens).map_err(|_| {
                    IronCrewError::Validation(
                        "PostgreSQL run-event terminal token count is negative".into(),
                    )
                })?;
                let expected_status = run_status.to_string();
                let terminal_data = terminal_entry
                    .payload
                    .get("data")
                    .and_then(serde_json::Value::as_object);
                let terminal_matches = terminal_data.is_some_and(|data| {
                    data.get("run_id").and_then(serde_json::Value::as_str)
                        == Some(batch.run_id.as_str())
                        && data.get("status").and_then(serde_json::Value::as_str)
                            == Some(expected_status.as_str())
                        && data.get("duration_ms").and_then(serde_json::Value::as_u64)
                            == Some(expected_duration_ms)
                        && data.get("total_tokens").and_then(serde_json::Value::as_u64)
                            == Some(u64::from(expected_total_tokens))
                });
                if !terminal_matches {
                    return Err(IronCrewError::Conflict(format!(
                        "run_complete for '{}' does not match its terminal run record",
                        batch.run_id
                    )));
                }
            }
        }

        let new_event_count = u64::try_from(new_entries.len())
            .map_err(|_| IronCrewError::Validation("Run-event append count exceeds u64".into()))?;
        let serialized_new_payloads = new_entries
            .iter()
            .map(|entry| {
                serde_json::to_string(&entry.payload).map_err(|error| {
                    IronCrewError::Validation(format!(
                        "Run-event payload serialization failed: {error}"
                    ))
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let declared_new_bytes = new_entries
            .iter()
            .map(|entry| {
                i64::try_from(entry.payload_bytes).map_err(|_| {
                    IronCrewError::Validation(
                        "Run-event payload byte count exceeds PostgreSQL BIGINT".into(),
                    )
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let accounted_new_bytes: Vec<i64> = if new_entries.is_empty() {
            Vec::new()
        } else {
            let accounting_sql = format!(
                "SELECT GREATEST(item.declared_bytes, \
                         octet_length(item.payload::jsonb::text)::BIGINT, \
                         {MIN_ACCOUNTED_RUN_EVENT_BYTES}::BIGINT) \
                 FROM unnest($1::text[], $2::bigint[]) WITH ORDINALITY \
                      AS item(payload, declared_bytes, ordinal) \
                 ORDER BY item.ordinal"
            );
            sqlx::query_scalar(sqlx::AssertSqlSafe(accounting_sql))
                .bind(&serialized_new_payloads)
                .bind(&declared_new_bytes)
                .fetch_all(&mut *tx)
                .await
                .map_err(|error| {
                    IronCrewError::Validation(format!(
                        "PostgreSQL run-event payload accounting failed: {error}"
                    ))
                })?
        };
        if accounted_new_bytes.len() != new_entries.len() {
            return Err(IronCrewError::Validation(
                "PostgreSQL run-event payload accounting returned an incomplete batch".into(),
            ));
        }
        let new_byte_count = accounted_new_bytes.iter().try_fold(0u64, |total, bytes| {
            let bytes = nonnegative_u64("accounted payload byte count", *bytes)?;
            total.checked_add(bytes).ok_or_else(|| {
                IronCrewError::Validation("Run-event append byte count overflow".into())
            })
        })?;
        if new_byte_count > self.run_event_journal_config.max_bytes_per_run as u64
            || new_byte_count > self.run_event_journal_config.max_total_bytes
        {
            return Err(IronCrewError::Validation(
                "PostgreSQL-accounted run-event append exceeds a configured byte limit".into(),
            ));
        }

        if new_event_count > 0 {
            let future_events = state
                .retained_events
                .checked_add(new_event_count)
                .ok_or_else(|| {
                    IronCrewError::Validation("Run-event per-run count overflow".into())
                })?;
            let future_bytes = state
                .retained_bytes
                .checked_add(new_byte_count)
                .ok_or_else(|| {
                    IronCrewError::Validation("Run-event per-run bytes overflow".into())
                })?;
            let events_to_free = future_events
                .saturating_sub(self.run_event_journal_config.max_events_per_run as u64);
            let bytes_to_free =
                future_bytes.saturating_sub(self.run_event_journal_config.max_bytes_per_run as u64);
            pruned.merge(
                self.evict_run_event_capacity(
                    &mut tx,
                    &batch.run_id,
                    events_to_free,
                    bytes_to_free,
                )
                .await?,
            );

            let usage = self.lock_run_event_usage(&mut tx).await?;
            let future_global_events = usage
                .retained_events
                .checked_add(new_event_count)
                .ok_or_else(|| {
                    IronCrewError::Validation("Run-event global count overflow".into())
                })?;
            let future_global_bytes = usage
                .retained_bytes
                .checked_add(new_byte_count)
                .ok_or_else(|| {
                    IronCrewError::Validation("Run-event global bytes overflow".into())
                })?;
            pruned.merge(
                self.evict_global_run_event_capacity(
                    &mut tx,
                    future_global_events
                        .saturating_sub(self.run_event_journal_config.max_total_events),
                    future_global_bytes
                        .saturating_sub(self.run_event_journal_config.max_total_bytes),
                )
                .await?,
            );
            state = self
                .run_event_state_for_update(&mut tx, &batch.run_id)
                .await?
                .ok_or_else(|| {
                    IronCrewError::Validation("PostgreSQL run-event state is missing".into())
                })?;

            let (created_at, expires_at) = self
                .database_clock_with_deadline(
                    &mut tx,
                    self.run_event_journal_config.retention.as_secs(),
                    "run-event retention",
                )
                .await?;
            let insert_sql = format!(
                "INSERT INTO {} (run_id, sequence, event_type, payload, payload_bytes, \
                     created_at, expires_at) \
                 VALUES ($1, $2, $3, $4::jsonb, $5, $6::timestamptz, $7::timestamptz)",
                self.run_events_table
            );
            for (entry, payload) in new_entries.iter().zip(&serialized_new_payloads) {
                sqlx::query(sqlx::AssertSqlSafe(insert_sql.clone()))
                    .bind(&batch.run_id)
                    .bind(i64::try_from(entry.sequence).map_err(|_| {
                        IronCrewError::Validation(
                            "Run-event sequence exceeds PostgreSQL BIGINT".into(),
                        )
                    })?)
                    .bind(&entry.event_type)
                    .bind(payload)
                    .bind(i64::try_from(entry.payload_bytes).map_err(|_| {
                        IronCrewError::Validation(
                            "Run-event payload byte count exceeds PostgreSQL BIGINT".into(),
                        )
                    })?)
                    .bind(&created_at)
                    .bind(&expires_at)
                    .execute(&mut *tx)
                    .await
                    .map_err(|error| {
                        IronCrewError::Validation(format!(
                            "PostgreSQL run-event append failed: {error}"
                        ))
                    })?;
            }

            let mut journal_complete = state.journal_complete;
            let mut expected_sequence = state.latest_sequence.saturating_add(1);
            let mut latest_sequence = state.latest_sequence;
            let mut terminal_event_sequence = state.terminal_event_sequence;
            for entry in &new_entries {
                if entry.sequence != expected_sequence {
                    journal_complete = false;
                }
                latest_sequence = latest_sequence.max(entry.sequence);
                expected_sequence = entry.sequence.saturating_add(1);
                if entry.event_type == "run_complete" {
                    terminal_event_sequence = Some(
                        terminal_event_sequence
                            .unwrap_or_default()
                            .max(entry.sequence),
                    );
                }
            }
            let retained_events = state
                .retained_events
                .checked_add(new_event_count)
                .ok_or_else(|| {
                    IronCrewError::Validation("Run-event retained count overflow".into())
                })?;
            let retained_bytes = state
                .retained_bytes
                .checked_add(new_byte_count)
                .ok_or_else(|| {
                    IronCrewError::Validation("Run-event retained bytes overflow".into())
                })?;
            let update_sql = format!(
                "UPDATE {} SET latest_sequence = $1, retained_events = $2, \
                     retained_bytes = $3, journal_complete = $4, \
                     terminal_event_sequence = $5, updated_at = clock_timestamp() \
                 WHERE run_id = $6",
                self.run_event_state_table
            );
            sqlx::query(sqlx::AssertSqlSafe(update_sql))
                .bind(i64::try_from(latest_sequence).map_err(|_| {
                    IronCrewError::Validation("Run-event latest sequence exceeds BIGINT".into())
                })?)
                .bind(i64::try_from(retained_events).map_err(|_| {
                    IronCrewError::Validation("Run-event retained count exceeds BIGINT".into())
                })?)
                .bind(i64::try_from(retained_bytes).map_err(|_| {
                    IronCrewError::Validation("Run-event retained bytes exceeds BIGINT".into())
                })?)
                .bind(journal_complete)
                .bind(
                    terminal_event_sequence
                        .map(i64::try_from)
                        .transpose()
                        .map_err(|_| {
                            IronCrewError::Validation(
                                "Run-event terminal sequence exceeds BIGINT".into(),
                            )
                        })?,
                )
                .bind(&batch.run_id)
                .execute(&mut *tx)
                .await
                .map_err(|error| {
                    IronCrewError::Validation(format!(
                        "PostgreSQL run-event state append update failed: {error}"
                    ))
                })?;
        }

        state = self
            .run_event_state_for_update(&mut tx, &batch.run_id)
            .await?
            .ok_or_else(|| {
                IronCrewError::Validation("PostgreSQL run-event state is missing".into())
            })?;
        let bounds = self
            .run_event_bounds(&mut tx, &batch.run_id, &state)
            .await?;
        let run_eviction = pruned.for_run(&batch.run_id);
        let outcome = RunEventAppendOutcome {
            appended_events: new_event_count,
            duplicate_events,
            evicted_events: run_eviction.events,
            evicted_bytes: run_eviction.bytes,
            eviction_gap: run_eviction.gap(),
            bounds,
        };
        outcome.validate()?;
        tx.commit().await.map_err(|error| {
            IronCrewError::Validation(format!(
                "PostgreSQL run-event append commit failed: {error}"
            ))
        })?;
        Ok(outcome)
    }

    async fn read_run_events(
        &self,
        flow: &str,
        run_id: &str,
        after_sequence: u64,
    ) -> Result<RunEventPage> {
        validate_human_input_route("flow", flow, 255)?;
        validate_human_input_route("run id", run_id, 128)?;
        let mut tx = self.pool.begin().await.map_err(|error| {
            IronCrewError::Validation(format!(
                "PostgreSQL run-event read transaction failed: {error}"
            ))
        })?;
        let run_sql = format!(
            "SELECT flow, status, duration_ms, total_tokens, \
                    to_char(clock_timestamp() AT TIME ZONE 'UTC', \
                        'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"') AS snapshot_at \
             FROM {} \
             WHERE run_id = $1 FOR SHARE",
            self.table_name
        );
        let run: Option<(String, String, i64, i32, String)> =
            sqlx::query_as(sqlx::AssertSqlSafe(run_sql))
                .bind(run_id)
                .fetch_optional(&mut *tx)
                .await
                .map_err(|error| {
                    IronCrewError::Validation(format!(
                        "PostgreSQL run-event run lookup failed: {error}"
                    ))
                })?;
        let (stored_flow, status, duration_ms, total_tokens, snapshot_at) =
            run.ok_or_else(|| IronCrewError::Validation(format!("Run '{run_id}' not found")))?;
        if stored_flow != flow {
            return Err(IronCrewError::Conflict(format!(
                "Run-event flow '{flow}' does not match run '{run_id}'"
            )));
        }
        // SSE polls are a high-frequency, multi-replica read path. A shared
        // state lock yields a consistent per-run snapshot without taking the
        // singleton accounting lock or doing retention writes. Logical
        // retention is applied below against one captured database timestamp;
        // append/reconciliation perform bounded physical cleanup.
        let state = self.run_event_state_for_share(&mut tx, run_id).await?;
        let (mut bounds, logical_gap_reason) = match &state {
            Some(state) => {
                let logical_bounds_sql = format!(
                    "SELECT \
                         MIN(sequence) FILTER (WHERE expires_at > $2::timestamptz), \
                         COUNT(*) FILTER (WHERE expires_at > $2::timestamptz)::BIGINT, \
                         COALESCE(SUM(accounted_bytes) FILTER (\
                             WHERE expires_at > $2::timestamptz), 0)::BIGINT, \
                         MAX(sequence) FILTER (WHERE expires_at <= $2::timestamptz) \
                     FROM {} WHERE run_id = $1",
                    self.run_events_table
                );
                let (earliest, retained_events, retained_bytes, latest_expired): (
                    Option<i64>,
                    i64,
                    i64,
                    Option<i64>,
                ) = sqlx::query_as(sqlx::AssertSqlSafe(logical_bounds_sql))
                    .bind(run_id)
                    .bind(&snapshot_at)
                    .fetch_one(&mut *tx)
                    .await
                    .map_err(|error| {
                        IronCrewError::Validation(format!(
                            "PostgreSQL logical run-event bounds lookup failed: {error}"
                        ))
                    })?;
                let retained_events =
                    nonnegative_u64("logical per-run retained event count", retained_events)?;
                let retained_bytes =
                    nonnegative_u64("logical per-run retained byte count", retained_bytes)?;
                let earliest_retained_sequence = earliest
                    .map(|value| nonnegative_u64("earliest retained sequence", value))
                    .transpose()?;
                let latest_expired = latest_expired
                    .map(|value| nonnegative_u64("latest expired sequence", value))
                    .transpose()?;
                let retention_boundary = match (latest_expired, earliest_retained_sequence) {
                    (Some(expired), Some(earliest)) => expired.min(earliest.saturating_sub(1)),
                    (Some(expired), None) => expired,
                    (None, _) => state.dropped_through,
                };
                let dropped_through = state.dropped_through.max(retention_boundary);
                let gap_reason = if dropped_through > state.dropped_through {
                    Some(RunEventGapReason::Retention)
                } else {
                    state.eviction_reason
                };
                let bounds = RunEventBounds {
                    earliest_retained_sequence,
                    latest_sequence: state.latest_sequence,
                    dropped_through,
                    retained_events,
                    retained_bytes,
                    journal_complete: state.journal_complete,
                };
                bounds.validate()?;
                (bounds, gap_reason)
            }
            None => (RunEventBounds::empty(), None),
        };
        if after_sequence > bounds.latest_sequence {
            return Err(IronCrewError::Validation(format!(
                "Run-event page starts ahead of latest sequence {}",
                bounds.latest_sequence
            )));
        }

        let effective_after = after_sequence.max(bounds.dropped_through);
        let event_sql = format!(
            "WITH candidate_sizes AS MATERIALIZED (\
                 SELECT sequence, accounted_bytes \
                 FROM {events} WHERE run_id = $1 AND sequence > $2 \
                   AND expires_at > $3::timestamptz \
                 ORDER BY sequence LIMIT $4\
             ), bounded_sequences AS (\
                 SELECT sequence, \
                        ROW_NUMBER() OVER (ORDER BY sequence) AS page_row, \
                        SUM(accounted_bytes) OVER (ORDER BY sequence \
                            ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) AS page_bytes \
                 FROM candidate_sizes\
             ) \
             SELECT event.sequence, event.event_type, event.payload::text AS payload, \
                    event.payload_bytes, \
                    to_char(event.created_at AT TIME ZONE 'UTC', \
                        'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"') AS created_at \
             FROM bounded_sequences AS bounded \
             JOIN {events} AS event ON event.run_id = $1 \
                  AND event.sequence = bounded.sequence \
             WHERE bounded.page_bytes <= $5 OR bounded.page_row = 1 \
             ORDER BY event.sequence",
            events = self.run_events_table,
        );
        let rows = sqlx::query(sqlx::AssertSqlSafe(event_sql))
            .bind(run_id)
            .bind(i64::try_from(effective_after).map_err(|_| {
                IronCrewError::Validation("Run-event page boundary exceeds BIGINT".into())
            })?)
            .bind(&snapshot_at)
            .bind(
                i64::try_from(self.run_event_journal_config.page_max_events).map_err(|_| {
                    IronCrewError::Validation("Run-event page limit exceeds BIGINT".into())
                })?,
            )
            .bind(
                i64::try_from(self.run_event_journal_config.page_max_bytes).map_err(|_| {
                    IronCrewError::Validation("Run-event page byte limit exceeds BIGINT".into())
                })?,
            )
            .fetch_all(&mut *tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL run-event page lookup failed: {error}"
                ))
            })?;

        let mut events = Vec::with_capacity(rows.len());
        for row in rows {
            let payload_bytes_i64: i64 = row.try_get("payload_bytes").map_err(|error| {
                IronCrewError::Validation(format!("Run-event payload_bytes column: {error}"))
            })?;
            let payload_bytes_u64 =
                nonnegative_u64("stored page payload byte count", payload_bytes_i64)?;
            let payload_bytes = usize::try_from(payload_bytes_u64).map_err(|_| {
                IronCrewError::Validation(
                    "Run-event stored payload byte count exceeds usize".into(),
                )
            })?;
            let payload_raw: String = row.try_get("payload").map_err(|error| {
                IronCrewError::Validation(format!("Run-event payload column: {error}"))
            })?;
            events.push(RunEventEntry {
                sequence: nonnegative_u64(
                    "page sequence",
                    row.try_get("sequence").map_err(|error| {
                        IronCrewError::Validation(format!("Run-event sequence column: {error}"))
                    })?,
                )?,
                event_type: row.try_get("event_type").map_err(|error| {
                    IronCrewError::Validation(format!("Run-event event_type column: {error}"))
                })?,
                payload: decode_stored_json(&payload_raw, "run_events.payload")?,
                payload_bytes,
                created_at: row.try_get("created_at").map_err(|error| {
                    IronCrewError::Validation(format!("Run-event created_at column: {error}"))
                })?,
            });
        }

        let mut gap = if after_sequence < bounds.dropped_through {
            let reason = logical_gap_reason.ok_or_else(|| {
                IronCrewError::Validation(
                    "PostgreSQL run-event dropped boundary has no reason".into(),
                )
            })?;
            // `RunEventPage` can describe one gap. Return this prefix gap by
            // itself so the caller advances to `dropped_through`; the next
            // read can then report a distinct internal writer gap without
            // emitting non-monotonic SSE ids.
            events.clear();
            Some(RunEventGap {
                first_sequence: after_sequence.saturating_add(1),
                last_sequence: bounds.dropped_through,
                reason,
            })
        } else {
            None
        };
        if gap.is_none() {
            let mut expected = after_sequence.saturating_add(1);
            let mut internal_gap = None;
            for (index, event) in events.iter().enumerate() {
                if event.sequence > expected {
                    internal_gap = Some((index, expected, event.sequence.saturating_sub(1)));
                    break;
                }
                expected = event.sequence.saturating_add(1);
            }
            if let Some((index, first_sequence, last_sequence)) = internal_gap {
                if index == 0 {
                    events.clear();
                    gap = Some(RunEventGap {
                        first_sequence,
                        last_sequence,
                        reason: RunEventGapReason::WriterBackpressure,
                    });
                } else {
                    // Return only the contiguous prefix. The caller advances
                    // through it and observes the internal gap on the next
                    // read, preserving the single-gap page contract.
                    events.truncate(index);
                }
            }
            if gap.is_none() && events.is_empty() && after_sequence < bounds.latest_sequence {
                gap = Some(RunEventGap {
                    first_sequence: after_sequence.saturating_add(1),
                    last_sequence: bounds.latest_sequence,
                    reason: RunEventGapReason::WriterBackpressure,
                });
            }
        }

        let status = status.parse::<RunStatus>()?;
        let terminal_event_sequence = state
            .as_ref()
            .and_then(|state| state.terminal_event_sequence);
        if status.is_terminal() && terminal_event_sequence.is_none() {
            bounds.journal_complete = false;
        }
        let terminal = if status.is_terminal() {
            Some(RunEventTerminalState {
                status,
                duration_ms: nonnegative_u64("terminal duration", duration_ms)?,
                total_tokens: u32::try_from(total_tokens).map_err(|_| {
                    IronCrewError::Validation(
                        "PostgreSQL run-event terminal token count is negative".into(),
                    )
                })?,
                event_sequence: terminal_event_sequence,
            })
        } else {
            None
        };
        let page = RunEventPage {
            run_id: run_id.to_owned(),
            after_sequence,
            events,
            bounds,
            gap,
            terminal,
        };
        page.validate(&self.run_event_journal_config)?;
        tx.commit().await.map_err(|error| {
            IronCrewError::Validation(format!("PostgreSQL run-event read commit failed: {error}"))
        })?;
        Ok(page)
    }

    async fn heartbeat_owned_runs(&self) -> Result<usize> {
        let mut tx = self.pool.begin().await.map_err(|error| {
            IronCrewError::Validation(format!("PG heartbeat transaction: {error}"))
        })?;
        self.lock_run_fence(&mut tx, true).await?;
        let (_, deadline) = self
            .database_clock_with_deadline(
                &mut tx,
                self.lease.ttl().as_secs(),
                "run heartbeat lease",
            )
            .await?;
        let sql = format!(
            "UPDATE {runs} AS run SET lease_expires_at = $1
             WHERE run.owner_instance_id = $2
               AND run.status IN ('running', 'waiting_for_input')
               AND NOT EXISTS (
                   SELECT 1 FROM {idempotency} AS idem
                   WHERE idem.operation = $3 AND idem.resource_id = run.run_id
               )",
            runs = self.table_name,
            idempotency = self.idempotency_table
        );
        let result = sqlx::query(sqlx::AssertSqlSafe(sql.to_string()))
            .bind(&deadline)
            .bind(self.lease.instance_id())
            .bind(RUN_OPERATION)
            .execute(&mut *tx)
            .await
            .map_err(|e| IronCrewError::Validation(format!("PG heartbeat: {}", e)))?;
        tx.commit()
            .await
            .map_err(|error| IronCrewError::Validation(format!("PG heartbeat commit: {error}")))?;
        Ok(result.rows_affected() as usize)
    }

    async fn health_check(&self) -> Result<()> {
        self.verify_required_schema().await?;

        // Exercise the write privilege used by heartbeat/finalization without
        // mutating a row. A read-only credential must never report ready.
        let mut transaction = self.pool.begin().await.map_err(|e| {
            IronCrewError::Validation(format!("PostgreSQL health transaction: {e}"))
        })?;
        let sql = format!(
            "UPDATE {} SET lease_expires_at = lease_expires_at WHERE FALSE",
            self.table_name
        );
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .execute(&mut *transaction)
            .await
            .map_err(|e| {
                IronCrewError::Validation(format!("PostgreSQL health write probe: {e}"))
            })?;
        let idempotency_sql = format!(
            "UPDATE {} SET updated_at = updated_at WHERE FALSE",
            self.idempotency_table
        );
        sqlx::query(sqlx::AssertSqlSafe(idempotency_sql))
            .execute(&mut *transaction)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL idempotency health write probe: {error}"
                ))
            })?;
        let accounting_sql = format!(
            "UPDATE {} SET updated_at = updated_at WHERE FALSE",
            self.idempotency_accounting_table
        );
        sqlx::query(sqlx::AssertSqlSafe(accounting_sql))
            .execute(&mut *transaction)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL idempotency accounting health write probe: {error}"
                ))
            })?;
        let human_input_sql = format!(
            "UPDATE {} SET expires_at = expires_at WHERE FALSE",
            self.human_inputs_table
        );
        sqlx::query(sqlx::AssertSqlSafe(human_input_sql))
            .execute(&mut *transaction)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL human-input mailbox health write probe: {error}"
                ))
            })?;
        for (table, column) in [
            (&self.run_events_table, "created_at"),
            (&self.run_event_state_table, "updated_at"),
            (&self.run_event_usage_table, "updated_at"),
        ] {
            let sql = format!("UPDATE {table} SET {column} = {column} WHERE FALSE");
            sqlx::query(sqlx::AssertSqlSafe(sql))
                .execute(&mut *transaction)
                .await
                .map_err(|error| {
                    IronCrewError::Validation(format!(
                        "PostgreSQL run-event journal health write probe failed for '{table}': {error}"
                    ))
                })?;
        }
        transaction
            .rollback()
            .await
            .map_err(|e| IronCrewError::Validation(format!("PostgreSQL health rollback: {e}")))?;
        Ok(())
    }

    async fn reconcile_abandoned_runs(&self, now: &str) -> Result<usize> {
        parse_timestamp("reconciliation timestamp", now)?;
        let mut tx = self.pool.begin().await.map_err(|error| {
            IronCrewError::Validation(format!("PG reconcile transaction: {error}"))
        })?;
        self.lock_idempotency_quota(&mut tx).await?;
        self.lock_run_fence(&mut tx, false).await?;
        let (database_now, _) = self
            .database_clock_with_deadline(&mut tx, 0, "run reconciliation")
            .await?;

        // A process may die after durably allocating/replying with a run id
        // but before publishing the normal run intent. Materialize a minimal
        // terminal run so retries can replay that id without re-executing and
        // callers can observe an explicit Abandoned outcome instead of 404.
        let fallback_sql = format!(
            "INSERT INTO {runs} (\
                 run_id, flow_name, flow, status, started_at, finished_at, duration_ms, \
                 task_results, agent_count, task_count, total_tokens, cached_tokens, tags, \
                 owner_instance_id, lease_expires_at\
             ) \
             SELECT idem.resource_id, idem.scope, idem.scope, 'abandoned', \
                    idem.created_at, $1, 0, '[]'::jsonb, 0, 0, 0, 0, '[]'::jsonb, \
                    idem.owner_instance_id, '' \
             FROM {idempotency} AS idem \
             WHERE idem.operation = $2 AND idem.state = 'claimed' \
               AND idem.lease_expires_at::timestamptz <= $3::timestamptz \
               AND NOT EXISTS (SELECT 1 FROM {runs} AS run WHERE run.run_id = idem.resource_id) \
             ON CONFLICT (run_id) DO NOTHING",
            runs = self.table_name,
            idempotency = self.idempotency_table
        );
        let inserted = sqlx::query(sqlx::AssertSqlSafe(fallback_sql))
            .bind(&database_now)
            .bind(RUN_OPERATION)
            .bind(&database_now)
            .execute(&mut *tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!("PG idempotent run fallback: {error}"))
            })?;

        let sql = format!(
            "UPDATE {}
             SET status = 'abandoned', finished_at = $1, lease_expires_at = ''
             WHERE status IN ('running', 'waiting_for_input')
               AND (lease_expires_at = '' \
                    OR lease_expires_at::timestamptz <= $2::timestamptz)",
            self.table_name
        );
        let result = sqlx::query(sqlx::AssertSqlSafe(sql.to_string()))
            .bind(&database_now)
            .bind(&database_now)
            .execute(&mut *tx)
            .await
            .map_err(|e| IronCrewError::Validation(format!("PG reconcile: {}", e)))?;

        let journal_sql = format!(
            "UPDATE {state} AS journal SET journal_complete = FALSE, \
                 updated_at = clock_timestamp() \
             FROM {runs} AS run \
             WHERE journal.run_id = run.run_id AND run.status = 'abandoned'",
            state = self.run_event_state_table,
            runs = self.table_name,
        );
        sqlx::query(sqlx::AssertSqlSafe(journal_sql))
            .execute(&mut *tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL abandoned run-event journal update failed: {error}"
                ))
            })?;

        let mapping_sql = format!(
            "UPDATE {idempotency} AS idem \
             SET state = 'completed', lease_expires_at = '', \
                 updated_at = $2, completed_at = $2, \
                 expires_at = to_char(\
                     ($2::timestamptz + idem.ttl_seconds * interval '1 second') \
                         AT TIME ZONE 'UTC', \
                     'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"'\
                 ) \
             FROM {runs} AS run \
             WHERE idem.operation = $1 AND idem.resource_id = run.run_id \
               AND idem.state IN ('claimed', 'running', 'indeterminate') \
               AND run.status NOT IN ('running', 'waiting_for_input')",
            idempotency = self.idempotency_table,
            runs = self.table_name
        );
        sqlx::query(sqlx::AssertSqlSafe(mapping_sql))
            .bind(RUN_OPERATION)
            .bind(&database_now)
            .execute(&mut *tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PG reconciled run idempotency transition: {error}"
                ))
            })?;

        // Conversation effects cannot be reconstructed after their exclusive
        // lease expires. Preserve a terminal tombstone so the same request is
        // never executed a second time.
        let conversation_sql = format!(
            "UPDATE {} SET state = 'indeterminate', response_status = NULL, \
             response_body = NULL, lease_expires_at = '', updated_at = $1, \
             completed_at = $1, expires_at = to_char(\
                 ($1::timestamptz + ttl_seconds * interval '1 second') AT TIME ZONE 'UTC', \
                 'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"'\
             ) \
             WHERE operation = $2 AND state IN ('claimed', 'running') \
               AND lease_expires_at::timestamptz <= $1::timestamptz",
            self.idempotency_table
        );
        sqlx::query(sqlx::AssertSqlSafe(conversation_sql))
            .bind(&database_now)
            .bind(CONVERSATION_MESSAGE_OPERATION)
            .execute(&mut *tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PG conversation idempotency reconciliation: {error}"
                ))
            })?;
        let human_input_cleanup = format!(
            "DELETE FROM {human_inputs} AS human \
             USING {runs} AS run \
             WHERE human.run_id = run.run_id AND (\
                 run.status NOT IN ('running', 'waiting_for_input') OR \
                 (human.state = 'pending' AND human.expires_at <= $1::timestamptz)\
             )",
            human_inputs = self.human_inputs_table,
            runs = self.table_name,
        );
        sqlx::query(sqlx::AssertSqlSafe(human_input_cleanup))
            .bind(&database_now)
            .execute(&mut *tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL reconciled human-input mailbox cleanup failed: {error}"
                ))
            })?;
        let reconciled = (inserted.rows_affected() + result.rows_affected()) as usize;
        tx.commit()
            .await
            .map_err(|error| IronCrewError::Validation(format!("PG reconcile commit: {error}")))?;
        // Keep journal cleanup outside the core reconciliation transaction so
        // usage-row contention or malformed journal data cannot undo critical
        // run/idempotency/HITL recovery.
        self.prune_expired_run_events_best_effort().await;
        Ok(reconciled)
    }

    async fn get_run(&self, run_id: &str) -> Result<RunRecord> {
        let sql = format!(
            "SELECT run_id, flow_name, flow, status, started_at, finished_at, duration_ms, task_results::text, agent_count, task_count, total_tokens, cached_tokens, tags::text, owner_instance_id, lease_expires_at
             FROM {} WHERE run_id = $1",
            self.table_name
        );

        let row = sqlx::query(sqlx::AssertSqlSafe(sql.to_string()))
            .bind(run_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| IronCrewError::Validation(format!("PostgreSQL query error: {}", e)))?
            .ok_or_else(|| IronCrewError::Validation(format!("Run '{}' not found", run_id)))?;

        row_to_record(&row)
    }

    async fn list_runs_summary(
        &self,
        filter: &ListRunsFilter,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<RunSummary>> {
        // Shared WHERE builder keeps the tag containment identical to the
        // SQLite backend. We NEVER select task_results — that's the whole
        // point of the summary view. LIMIT/OFFSET stay inline (trusted
        // integers) so the builder's `$N` numbering is left undisturbed.
        let WhereClause {
            sql: where_sql,
            params,
        } = store_sql::runs_where(filter, Dialect::Postgres);
        let mut sql = format!(
            "SELECT run_id, flow_name, flow, status, started_at, finished_at, duration_ms, \
             agent_count, task_count, total_tokens, cached_tokens, tags::text \
             FROM {}{}",
            self.table_name, where_sql
        );
        sql.push_str(" ORDER BY started_at DESC");
        if limit > 0 {
            sql.push_str(&format!(" LIMIT {}", limit as i64));
            if offset > 0 {
                sql.push_str(&format!(" OFFSET {}", offset as i64));
            }
        }

        let query = sqlx::query(sqlx::AssertSqlSafe(sql.to_string()));
        let query = bind_params(query, &params);

        let rows = query
            .fetch_all(&self.pool)
            .await
            .map_err(|e| IronCrewError::Validation(format!("PostgreSQL query error: {}", e)))?;

        rows.iter().map(row_to_summary).collect()
    }

    async fn count_runs(&self, filter: &ListRunsFilter) -> Result<u64> {
        let WhereClause {
            sql: where_sql,
            params,
        } = store_sql::runs_where(filter, Dialect::Postgres);
        let sql = format!("SELECT COUNT(*) FROM {}{}", self.table_name, where_sql);

        let query = sqlx::query(sqlx::AssertSqlSafe(sql.to_string()));
        let query = bind_params(query, &params);

        let row = query
            .fetch_one(&self.pool)
            .await
            .map_err(|e| IronCrewError::Validation(format!("PostgreSQL count error: {}", e)))?;
        let count: i64 = row
            .try_get(0)
            .map_err(|e| IronCrewError::Validation(format!("Column error: {}", e)))?;
        Ok(count as u64)
    }

    async fn delete_run(&self, run_id: &str) -> Result<()> {
        let sql = format!("DELETE FROM {} WHERE run_id = $1", self.table_name);
        let mut tx = self.pool.begin().await.map_err(|error| {
            IronCrewError::Validation(format!("PostgreSQL delete transaction: {error}"))
        })?;
        // Cascading event deletion fires the global accounting trigger. Take
        // the same lock order as append/read to prevent a run-row/usage-row
        // deadlock and keep exact counters observable throughout the delete.
        self.lock_run_event_usage(&mut tx).await?;
        let result = sqlx::query(sqlx::AssertSqlSafe(sql.to_string()))
            .bind(run_id)
            .execute(&mut *tx)
            .await
            .map_err(|e| IronCrewError::Validation(format!("PostgreSQL delete error: {}", e)))?;

        if result.rows_affected() == 0 {
            return Err(IronCrewError::Validation(format!(
                "Run '{}' not found",
                run_id
            )));
        }
        tx.commit().await.map_err(|error| {
            IronCrewError::Validation(format!("PostgreSQL delete commit: {error}"))
        })?;
        Ok(())
    }

    async fn lookup_idempotency_for_principal(
        &self,
        principal_id: &PrincipalId,
        key_hash: &str,
        request_fingerprint: &str,
        now: &str,
    ) -> Result<IdempotencyLookup> {
        principal_id.validate()?;
        validate_digest("idempotency key hash", key_hash)?;
        validate_digest("request fingerprint", request_fingerprint)?;
        parse_timestamp("idempotency lookup time", now)?;
        let mut tx = self.pool.begin().await.map_err(|error| {
            IronCrewError::Validation(format!(
                "PostgreSQL idempotency lookup transaction failed: {error}"
            ))
        })?;
        self.lock_idempotency_key(&mut tx, key_hash).await?;
        let (database_now, _) = self
            .database_clock_with_deadline(&mut tx, 0, "idempotency lookup")
            .await?;
        let now_timestamp = parse_timestamp("PostgreSQL idempotency clock", &database_now)?;

        let Some(record) = self
            .get_idempotency_in_transaction(&mut tx, key_hash)
            .await?
        else {
            tx.commit().await.map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL idempotency lookup commit failed: {error}"
                ))
            })?;
            return Ok(IdempotencyLookup::Miss);
        };

        let outcome = if &record.principal_id != principal_id
            || record.request_fingerprint != request_fingerprint
        {
            IdempotencyLookup::Conflict
        } else if record.state.is_terminal()
            && record
                .expires_at
                .as_deref()
                .map(|expires_at| {
                    parse_timestamp("stored idempotency retention expiry", expires_at)
                        .map(|expires_at| expires_at <= now_timestamp)
                })
                .transpose()?
                .unwrap_or(false)
        {
            IdempotencyLookup::Miss
        } else if record.state == IdempotencyState::Indeterminate {
            IdempotencyLookup::Indeterminate(record)
        } else if record.replayable() {
            IdempotencyLookup::Replay(record)
        } else {
            match record.state {
                IdempotencyState::Claimed | IdempotencyState::Running => {
                    let lease = parse_timestamp(
                        "stored idempotency lease expiry",
                        &record.lease_expires_at,
                    )?;
                    if lease > now_timestamp {
                        IdempotencyLookup::InProgress(record)
                    } else {
                        IdempotencyLookup::Indeterminate(record)
                    }
                }
                IdempotencyState::Completed | IdempotencyState::Indeterminate => {
                    IdempotencyLookup::Indeterminate(record)
                }
            }
        };
        tx.commit().await.map_err(|error| {
            IronCrewError::Validation(format!(
                "PostgreSQL idempotency lookup commit failed: {error}"
            ))
        })?;
        Ok(outcome)
    }

    async fn claim_idempotency_with_limits(
        &self,
        claim: IdempotencyClaim,
        limits: IdempotencyLimits,
    ) -> Result<IdempotencyClaimOutcome> {
        claim.validate()?;
        limits.validate()?;
        let mut tx = self.pool.begin().await.map_err(|error| {
            IronCrewError::Validation(format!(
                "PostgreSQL idempotency claim transaction failed: {error}"
            ))
        })?;
        // Every capacity mutation follows one order: global quota, principal,
        // optional run fence, resource, exclusive scope, and finally key.
        self.lock_idempotency_quota(&mut tx).await?;
        self.lock_idempotency_principal(&mut tx, &claim.principal_id)
            .await?;
        if claim.operation == RUN_OPERATION {
            self.lock_run_fence(&mut tx, true).await?;
        }
        self.lock_resource(
            &mut tx,
            &claim.operation,
            if claim.operation == RUN_OPERATION {
                ""
            } else {
                &claim.scope
            },
            &claim.resource_id,
        )
        .await?;
        let mut recovery_hazard_key = None;
        if let Some(exclusive_scope) = claim.exclusive_scope.as_deref() {
            self.lock_idempotency_scope(&mut tx, exclusive_scope)
                .await?;
        }
        self.lock_idempotency_key(&mut tx, &claim.key_hash).await?;
        let (database_now, lease_expires_at) = self
            .database_clock_with_deadline(&mut tx, self.lease.ttl().as_secs(), "idempotency claim")
            .await?;
        let database_timestamp = parse_timestamp("PostgreSQL idempotency clock", &database_now)?;

        if limits.prune_batch > 0 {
            let sql = format!(
                "DELETE FROM {table} WHERE key_hash IN (\
                     SELECT key_hash FROM {table} \
                     WHERE state IN ('completed', 'indeterminate') \
                       AND expires_at IS NOT NULL \
                       AND expires_at::timestamptz <= $1::timestamptz \
                     ORDER BY expires_at::timestamptz, key_hash LIMIT $2\
                 )",
                table = self.idempotency_table
            );
            sqlx::query(sqlx::AssertSqlSafe(sql))
                .bind(&database_now)
                .bind(i64::try_from(limits.prune_batch).unwrap_or(i64::MAX))
                .execute(&mut *tx)
                .await
                .map_err(|error| {
                    IronCrewError::Validation(format!(
                        "PostgreSQL idempotency claim pruning failed: {error}"
                    ))
                })?;
        }

        if let Some(record) = self
            .get_idempotency_in_transaction(&mut tx, &claim.key_hash)
            .await?
        {
            let expired_terminal = record.state.is_terminal()
                && record
                    .expires_at
                    .as_deref()
                    .map(|expires_at| {
                        parse_timestamp("stored idempotency retention expiry", expires_at)
                            .map(|expires_at| expires_at <= database_timestamp)
                    })
                    .transpose()?
                    .unwrap_or(false);
            if expired_terminal {
                let sql = format!("DELETE FROM {} WHERE key_hash = $1", self.idempotency_table);
                sqlx::query(sqlx::AssertSqlSafe(sql))
                    .bind(&claim.key_hash)
                    .execute(&mut *tx)
                    .await
                    .map_err(|error| {
                        IronCrewError::Validation(format!(
                            "PostgreSQL expired idempotency delete failed: {error}"
                        ))
                    })?;
            } else {
                let outcome = if record.principal_id != claim.principal_id
                    || record.request_fingerprint != claim.request_fingerprint
                {
                    IdempotencyClaimOutcome::Conflict
                } else if record.state == IdempotencyState::Indeterminate {
                    IdempotencyClaimOutcome::Indeterminate(record)
                } else if record.replayable() {
                    IdempotencyClaimOutcome::Replay(record)
                } else if record.state.is_in_flight()
                    && parse_timestamp("stored idempotency lease expiry", &record.lease_expires_at)?
                        <= database_timestamp
                {
                    let record = self
                        .mark_record_indeterminate_in_transaction(&mut tx, record)
                        .await?;
                    IdempotencyClaimOutcome::Indeterminate(record)
                } else if record.state.is_in_flight() {
                    IdempotencyClaimOutcome::InProgress(record)
                } else {
                    IdempotencyClaimOutcome::Indeterminate(record)
                };
                tx.commit().await.map_err(|error| {
                    IronCrewError::Validation(format!(
                        "PostgreSQL idempotency claim commit failed: {error}"
                    ))
                })?;
                return Ok(outcome);
            }
        }

        if claim.operation == CONVERSATION_MESSAGE_OPERATION {
            let expected_revision = claim.base_revision.ok_or_else(|| {
                IronCrewError::Validation(
                    "Conversation idempotency claim has no base revision".into(),
                )
            })?;
            let revision_sql = format!(
                "SELECT revision FROM {} \
                 WHERE id = $1 AND flow_path IS NOT DISTINCT FROM $2 FOR UPDATE",
                self.conversations_table
            );
            let current_revision: Option<i64> =
                sqlx::query_scalar(sqlx::AssertSqlSafe(revision_sql))
                    .bind(&claim.resource_id)
                    .bind(&claim.scope)
                    .fetch_optional(&mut *tx)
                    .await
                    .map_err(|error| {
                        IronCrewError::Validation(format!(
                            "PostgreSQL conversation idempotency revision query failed: {error}"
                        ))
                    })?;
            let current_revision = current_revision
                .map(u64::try_from)
                .transpose()
                .map_err(|_| {
                    IronCrewError::Validation("PostgreSQL conversation revision is negative".into())
                })?
                .unwrap_or(0);
            if current_revision != expected_revision {
                tx.commit().await.map_err(|error| {
                    IronCrewError::Validation(format!(
                        "PostgreSQL conversation idempotency conflict commit failed: {error}"
                    ))
                })?;
                return Ok(IdempotencyClaimOutcome::Conflict);
            }
        }

        if let Some(exclusive_scope) = claim.exclusive_scope.as_deref() {
            let sql = format!(
                "SELECT {IDEMPOTENCY_COLUMNS} FROM {} \
                 WHERE exclusive_scope = $1 AND key_hash <> $2 \
                   AND state IN ('claimed', 'running') FOR UPDATE",
                self.idempotency_table
            );
            if let Some(row) = sqlx::query(sqlx::AssertSqlSafe(sql))
                .bind(exclusive_scope)
                .bind(&claim.key_hash)
                .fetch_optional(&mut *tx)
                .await
                .map_err(|error| {
                    IronCrewError::Validation(format!(
                        "PostgreSQL idempotency exclusive-scope lookup failed: {error}"
                    ))
                })?
            {
                let record = row_to_idempotency_record(&row)?;
                let lease =
                    parse_timestamp("stored idempotency lease expiry", &record.lease_expires_at)?;
                if lease > database_timestamp {
                    tx.commit().await.map_err(|error| {
                        IronCrewError::Validation(format!(
                            "PostgreSQL idempotency busy commit failed: {error}"
                        ))
                    })?;
                    return Ok(IdempotencyClaimOutcome::Busy);
                }
                self.mark_record_indeterminate_in_transaction(&mut tx, record)
                    .await?;
                tx.commit().await.map_err(|error| {
                    IronCrewError::Validation(format!(
                        "PostgreSQL expired idempotency barrier commit failed: {error}"
                    ))
                })?;
                return Ok(IdempotencyClaimOutcome::Busy);
            }

            let hazard_sql = format!(
                "SELECT key_hash, principal_id, completed_at FROM {} \
                 WHERE exclusive_scope = $1 AND key_hash <> $2 \
                   AND state = 'indeterminate' \
                 ORDER BY completed_at, key_hash LIMIT 2 FOR UPDATE",
                self.idempotency_table
            );
            let hazard_rows = sqlx::query(sqlx::AssertSqlSafe(hazard_sql))
                .bind(exclusive_scope)
                .bind(&claim.key_hash)
                .fetch_all(&mut *tx)
                .await
                .map_err(|error| {
                    IronCrewError::Validation(format!(
                        "PostgreSQL indeterminate exclusive-scope lookup failed: {error}"
                    ))
                })?;
            if !hazard_rows.is_empty() {
                let hazard = if hazard_rows.len() == 1 {
                    let key_hash =
                        hazard_rows[0]
                            .try_get::<String, _>("key_hash")
                            .map_err(|error| {
                                IronCrewError::Validation(format!(
                                    "PostgreSQL idempotency hazard key decode failed: {error}"
                                ))
                            })?;
                    let principal_id = hazard_rows[0]
                        .try_get::<String, _>("principal_id")
                        .map(PrincipalId::from_digest)
                        .map_err(|error| {
                            IronCrewError::Validation(format!(
                                "PostgreSQL idempotency hazard principal decode failed: {error}"
                            ))
                        })??;
                    let completed_at = hazard_rows[0]
                        .try_get::<Option<String>, _>("completed_at")
                        .map_err(|error| {
                            IronCrewError::Validation(format!(
                                "PostgreSQL idempotency hazard timestamp decode failed: {error}"
                            ))
                        })?;
                    Some((key_hash, principal_id, completed_at))
                } else {
                    None
                };
                let grace = chrono::Duration::from_std(self.lease.ttl()).map_err(|_| {
                    IronCrewError::Validation(
                        "PostgreSQL idempotency recovery grace is out of range".into(),
                    )
                })?;
                let grace_elapsed = hazard
                    .as_ref()
                    .and_then(|(_, _, completed_at)| completed_at.as_deref())
                    .map(|completed_at| {
                        parse_timestamp("stored idempotency hazard completion", completed_at)
                    })
                    .transpose()?
                    .and_then(|completed_at| completed_at.checked_add_signed(grace))
                    .is_some_and(|recovery_at| recovery_at <= database_timestamp);
                let recoverable = hazard.as_ref().is_some_and(|(key_hash, principal_id, _)| {
                    principal_id == &claim.principal_id
                        && claim.recovery_key_hash.as_deref() == Some(key_hash.as_str())
                }) && grace_elapsed;
                if !recoverable {
                    tx.commit().await.map_err(|error| {
                        IronCrewError::Validation(format!(
                            "PostgreSQL idempotency hazard commit failed: {error}"
                        ))
                    })?;
                    return Ok(IdempotencyClaimOutcome::Busy);
                }
                let recovery_key_hash = claim.recovery_key_hash.as_deref().ok_or_else(|| {
                    IronCrewError::Validation("Missing idempotency recovery key".into())
                })?;
                // Keep the locked hazard bound until every quota check has
                // passed. A quota-denied transaction is committed so bounded
                // pruning/accounting can progress; clearing here would make
                // that commit consume the recovery capability without
                // inserting its successor.
                recovery_hazard_key = Some(recovery_key_hash.to_string());
            }
        }

        let (global_usage, principal_usage) = self
            .idempotency_accounting_for_update(&mut tx, &claim.principal_id)
            .await?;
        let quota = if global_usage.records >= limits.global_max_records {
            Some((
                IdempotencyQuotaScope::Global,
                IdempotencyQuotaResource::Records,
                None,
            ))
        } else if principal_usage.records >= limits.principal_max_records {
            Some((
                IdempotencyQuotaScope::Principal,
                IdempotencyQuotaResource::Records,
                Some(&claim.principal_id),
            ))
        } else if principal_usage.in_flight >= limits.principal_max_in_flight {
            Some((
                IdempotencyQuotaScope::Principal,
                IdempotencyQuotaResource::InFlight,
                Some(&claim.principal_id),
            ))
        } else {
            None
        };
        if let Some((scope, resource, retry_principal)) = quota {
            let retry_after_seconds = self
                .idempotency_retry_after_seconds(&mut tx, retry_principal, resource, &database_now)
                .await?;
            tx.commit().await.map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL idempotency quota commit failed: {error}"
                ))
            })?;
            return Ok(IdempotencyClaimOutcome::QuotaExceeded {
                scope,
                resource,
                retry_after_seconds,
            });
        }

        let mut record = claim.to_record();
        record.lease_expires_at = lease_expires_at;
        record.created_at = database_now.clone();
        record.updated_at = database_now;
        if let Some(response_body) = record.response_body.as_ref() {
            let response_fits = global_usage
                .response_bytes
                .checked_add(response_body.len())
                .is_some_and(|total| total <= limits.global_max_response_bytes)
                && principal_usage
                    .response_bytes
                    .checked_add(response_body.len())
                    .is_some_and(|total| total <= limits.principal_max_response_bytes);
            if !response_fits {
                record.response_body = None;
            }
        }
        record.validate()?;
        if let Some(hazard_key) = recovery_hazard_key.as_deref() {
            let clear_sql = format!(
                "UPDATE {} SET exclusive_scope = NULL, updated_at = $1 \
                 WHERE key_hash = $2 AND exclusive_scope = $3 \
                   AND principal_id = $4 AND state = 'indeterminate'",
                self.idempotency_table
            );
            let cleared = sqlx::query(sqlx::AssertSqlSafe(clear_sql))
                .bind(&record.created_at)
                .bind(hazard_key)
                .bind(record.exclusive_scope.as_deref())
                .bind(record.principal_id.as_str())
                .execute(&mut *tx)
                .await
                .map_err(|error| {
                    IronCrewError::Validation(format!(
                        "PostgreSQL idempotency hazard recovery failed: {error}"
                    ))
                })?;
            if cleared.rows_affected() != 1 {
                return Err(IronCrewError::Conflict(
                    "Idempotency recovery hazard changed before claim insertion".into(),
                ));
            }
        }
        let base_revision = record
            .base_revision
            .map(i64::try_from)
            .transpose()
            .map_err(|_| {
                IronCrewError::Validation(
                    "Idempotency base revision is out of PostgreSQL range".into(),
                )
            })?;
        let sql = format!(
            "INSERT INTO {} ({IDEMPOTENCY_COLUMNS}) VALUES (\
                 $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, 'claimed', \
                 $11, $12, $13, $14, $14, NULL, NULL, $15\
             )",
            self.idempotency_table
        );
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(&record.key_hash)
            .bind(record.principal_id.as_str())
            .bind(&record.request_fingerprint)
            .bind(&record.operation)
            .bind(&record.scope)
            .bind(&record.resource_id)
            .bind(&record.exclusive_scope)
            .bind(&record.attempt_id)
            .bind(&record.owner_instance_id)
            .bind(base_revision)
            .bind(record.response_status.map(i32::from))
            .bind(&record.response_body)
            .bind(&record.lease_expires_at)
            .bind(&record.created_at)
            .bind(i64::try_from(record.ttl_seconds).map_err(|_| {
                IronCrewError::Validation("Idempotency TTL is out of PostgreSQL range".into())
            })?)
            .execute(&mut *tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL idempotency claim insert failed: {error}"
                ))
            })?;
        tx.commit().await.map_err(|error| {
            IronCrewError::Validation(format!(
                "PostgreSQL idempotency claim commit failed: {error}"
            ))
        })?;
        Ok(IdempotencyClaimOutcome::Claimed(record))
    }

    async fn heartbeat_idempotency(
        &self,
        key_hash: &str,
        attempt_id: &str,
        new_lease_expires_at: &str,
    ) -> Result<bool> {
        validate_digest("idempotency key hash", key_hash)?;
        if attempt_id.is_empty() || attempt_id.len() > 128 {
            return Err(IronCrewError::Validation(
                "Idempotency attempt id must be 1..=128 bytes".into(),
            ));
        }
        parse_timestamp("idempotency heartbeat lease expiry", new_lease_expires_at)?;
        let mut tx = self.pool.begin().await.map_err(|error| {
            IronCrewError::Validation(format!(
                "PostgreSQL idempotency heartbeat transaction failed: {error}"
            ))
        })?;
        self.lock_idempotency_key(&mut tx, key_hash).await?;
        let (database_now, database_deadline) = self
            .database_clock_with_deadline(
                &mut tx,
                self.lease.ttl().as_secs(),
                "idempotency heartbeat",
            )
            .await?;
        let Some(record) = self
            .get_idempotency_in_transaction(&mut tx, key_hash)
            .await?
        else {
            return Ok(false);
        };
        if record.attempt_id != attempt_id {
            return Err(IronCrewError::Conflict(
                "Idempotency attempt changed before heartbeat".into(),
            ));
        }
        if !record.state.is_in_flight() {
            return Ok(record.state == IdempotencyState::Completed);
        }
        let sql = format!(
            "UPDATE {} SET lease_expires_at = $1, updated_at = $2 \
             WHERE key_hash = $3 AND attempt_id = $4 \
               AND state IN ('claimed', 'running')",
            self.idempotency_table
        );
        let result = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(&database_deadline)
            .bind(&database_now)
            .bind(key_hash)
            .bind(attempt_id)
            .execute(&mut *tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL idempotency heartbeat failed: {error}"
                ))
            })?;
        tx.commit().await.map_err(|error| {
            IronCrewError::Validation(format!(
                "PostgreSQL idempotency heartbeat commit failed: {error}"
            ))
        })?;
        Ok(result.rows_affected() == 1)
    }

    async fn heartbeat_idempotent_run(
        &self,
        run_id: &str,
        key_hash: &str,
        attempt_id: &str,
        new_lease_expires_at: &str,
    ) -> Result<RunFenceHeartbeat> {
        validate_digest("idempotency key hash", key_hash)?;
        if run_id.is_empty() || run_id.len() > 128 {
            return Err(IronCrewError::Validation(
                "Idempotent run id must be 1..=128 bytes".into(),
            ));
        }
        if attempt_id.is_empty() || attempt_id.len() > 128 {
            return Err(IronCrewError::Validation(
                "Idempotency attempt id must be 1..=128 bytes".into(),
            ));
        }
        // The absolute caller deadline remains part of the shared backend
        // contract, but PostgreSQL uses only its own clock for lease ordering.
        parse_timestamp(
            "idempotent run heartbeat lease expiry",
            new_lease_expires_at,
        )?;
        let mut tx = self.pool.begin().await.map_err(|error| {
            IronCrewError::Validation(format!(
                "PostgreSQL idempotent run heartbeat transaction failed: {error}"
            ))
        })?;
        self.lock_run_fence(&mut tx, true).await?;
        self.lock_resource(&mut tx, RUN_OPERATION, "", run_id)
            .await?;
        self.lock_idempotency_key(&mut tx, key_hash).await?;
        let (database_now, database_deadline) = self
            .database_clock_with_deadline(
                &mut tx,
                self.lease.ttl().as_secs(),
                "idempotent run heartbeat",
            )
            .await?;
        let Some(record) = self
            .get_idempotency_in_transaction(&mut tx, key_hash)
            .await?
        else {
            tx.commit().await.map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL missing run fence heartbeat commit failed: {error}"
                ))
            })?;
            return Ok(RunFenceHeartbeat::Lost);
        };
        if record.attempt_id != attempt_id {
            return Err(IronCrewError::Conflict(
                "Idempotency attempt changed before run heartbeat".into(),
            ));
        }
        if record.operation != RUN_OPERATION
            || record.resource_id != run_id
            || record.owner_instance_id != self.lease.instance_id()
        {
            tx.commit().await.map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL mismatched run fence heartbeat commit failed: {error}"
                ))
            })?;
            return Ok(RunFenceHeartbeat::Lost);
        }
        let cancellation_sql = format!(
            "SELECT cancel_requested_at FROM {} WHERE key_hash = $1",
            self.idempotency_table
        );
        let cancel_requested_at: Option<String> =
            sqlx::query_scalar(sqlx::AssertSqlSafe(cancellation_sql))
                .bind(key_hash)
                .fetch_one(&mut *tx)
                .await
                .map_err(|error| {
                    IronCrewError::Validation(format!(
                        "PostgreSQL idempotent run cancellation lookup failed: {error}"
                    ))
                })?;

        let run_sql = format!(
            "SELECT status, owner_instance_id, flow FROM {} WHERE run_id = $1 FOR UPDATE",
            self.table_name
        );
        let run = sqlx::query(sqlx::AssertSqlSafe(run_sql))
            .bind(run_id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL idempotent heartbeat run lookup failed: {error}"
                ))
            })?;

        let Some(run) = run else {
            if record.state != IdempotencyState::Claimed {
                tx.commit().await.map_err(|error| {
                    IronCrewError::Validation(format!(
                        "PostgreSQL lost run fence heartbeat commit failed: {error}"
                    ))
                })?;
                return Ok(RunFenceHeartbeat::Lost);
            }
            if cancel_requested_at.is_some() {
                tx.commit().await.map_err(|error| {
                    IronCrewError::Validation(format!(
                        "PostgreSQL claimed run cancellation commit failed: {error}"
                    ))
                })?;
                return Ok(RunFenceHeartbeat::CancelRequested);
            }
            let ledger_sql = format!(
                "UPDATE {} SET lease_expires_at = $1, updated_at = $2 \
                 WHERE key_hash = $3 AND operation = $4 AND resource_id = $5 \
                   AND attempt_id = $6 AND owner_instance_id = $7 \
                   AND state = 'claimed'",
                self.idempotency_table
            );
            let renewed = sqlx::query(sqlx::AssertSqlSafe(ledger_sql))
                .bind(&database_deadline)
                .bind(&database_now)
                .bind(key_hash)
                .bind(RUN_OPERATION)
                .bind(run_id)
                .bind(attempt_id)
                .bind(self.lease.instance_id())
                .execute(&mut *tx)
                .await
                .map_err(|error| {
                    IronCrewError::Validation(format!(
                        "PostgreSQL claimed run fence heartbeat failed: {error}"
                    ))
                })?;
            let outcome = if renewed.rows_affected() == 1 {
                RunFenceHeartbeat::Owned
            } else {
                RunFenceHeartbeat::Lost
            };
            tx.commit().await.map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL claimed run fence heartbeat commit failed: {error}"
                ))
            })?;
            return Ok(outcome);
        };

        let run_owner: String = run
            .try_get("owner_instance_id")
            .map_err(|error| IronCrewError::Validation(format!("Column error: {error}")))?;
        let run_flow: String = run
            .try_get("flow")
            .map_err(|error| IronCrewError::Validation(format!("Column error: {error}")))?;
        if run_owner != self.lease.instance_id() || run_flow != record.scope {
            tx.commit().await.map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL unowned run fence heartbeat commit failed: {error}"
                ))
            })?;
            return Ok(RunFenceHeartbeat::Lost);
        }
        let status = run
            .try_get::<String, _>("status")
            .map_err(|error| IronCrewError::Validation(format!("Column error: {error}")))?
            .parse::<RunStatus>()?;
        if status.is_terminal() {
            self.delete_human_inputs_for_run(&mut tx, run_id).await?;
            tx.commit().await.map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL terminal run fence heartbeat commit failed: {error}"
                ))
            })?;
            return Ok(RunFenceHeartbeat::Terminal(status));
        }
        if record.state != IdempotencyState::Running || !status.is_in_flight() {
            tx.commit().await.map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL lost run fence heartbeat commit failed: {error}"
                ))
            })?;
            return Ok(RunFenceHeartbeat::Lost);
        }
        if cancel_requested_at.is_some() {
            tx.commit().await.map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL running cancellation request commit failed: {error}"
                ))
            })?;
            return Ok(RunFenceHeartbeat::CancelRequested);
        }

        let run_update_sql = format!(
            "UPDATE {} SET lease_expires_at = $1 \
             WHERE run_id = $2 AND owner_instance_id = $3 \
               AND status IN ('running', 'waiting_for_input')",
            self.table_name
        );
        let run_renewed = sqlx::query(sqlx::AssertSqlSafe(run_update_sql))
            .bind(&database_deadline)
            .bind(run_id)
            .bind(self.lease.instance_id())
            .execute(&mut *tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL idempotent run lease heartbeat failed: {error}"
                ))
            })?;
        let ledger_update_sql = format!(
            "UPDATE {} SET lease_expires_at = $1, updated_at = $2 \
             WHERE key_hash = $3 AND operation = $4 AND resource_id = $5 \
               AND attempt_id = $6 AND owner_instance_id = $7 \
               AND state = 'running'",
            self.idempotency_table
        );
        let ledger_renewed = sqlx::query(sqlx::AssertSqlSafe(ledger_update_sql))
            .bind(&database_deadline)
            .bind(&database_now)
            .bind(key_hash)
            .bind(RUN_OPERATION)
            .bind(run_id)
            .bind(attempt_id)
            .bind(self.lease.instance_id())
            .execute(&mut *tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL idempotent run ledger heartbeat failed: {error}"
                ))
            })?;
        if run_renewed.rows_affected() != 1 || ledger_renewed.rows_affected() != 1 {
            tx.rollback().await.map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL lost run fence heartbeat rollback failed: {error}"
                ))
            })?;
            return Ok(RunFenceHeartbeat::Lost);
        }
        tx.commit().await.map_err(|error| {
            IronCrewError::Validation(format!(
                "PostgreSQL idempotent run heartbeat commit failed: {error}"
            ))
        })?;
        Ok(RunFenceHeartbeat::Owned)
    }

    async fn request_run_cancellation(
        &self,
        run_id: &str,
        flow: &str,
    ) -> Result<RunCancellationRequest> {
        if run_id.is_empty() || run_id.len() > 128 {
            return Err(IronCrewError::Validation(
                "Cancellation run id must be 1..=128 bytes".into(),
            ));
        }
        if flow.is_empty() || flow.len() > 255 || flow.chars().any(char::is_control) {
            return Err(IronCrewError::Validation(
                "Cancellation flow must be 1..=255 printable bytes".into(),
            ));
        }

        let mut tx = self.pool.begin().await.map_err(|error| {
            IronCrewError::Validation(format!(
                "PostgreSQL run cancellation transaction failed: {error}"
            ))
        })?;
        // Match the run-intent/heartbeat lock order. This serializes the
        // cancellation request with terminalization without blocking other
        // unrelated runs.
        self.lock_run_fence(&mut tx, true).await?;
        self.lock_resource(&mut tx, RUN_OPERATION, "", run_id)
            .await?;

        let run_sql = format!(
            "SELECT status, owner_instance_id, flow FROM {} WHERE run_id = $1 FOR UPDATE",
            self.table_name
        );
        let Some(run) = sqlx::query(sqlx::AssertSqlSafe(run_sql))
            .bind(run_id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL cancellation run lookup failed: {error}"
                ))
            })?
        else {
            tx.commit().await.map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL missing cancellation run commit failed: {error}"
                ))
            })?;
            return Ok(RunCancellationRequest::NotFound);
        };
        let run_flow: String = run
            .try_get("flow")
            .map_err(|error| IronCrewError::Validation(format!("Column error: {error}")))?;
        if run_flow != flow {
            tx.commit().await.map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL scoped cancellation lookup commit failed: {error}"
                ))
            })?;
            return Ok(RunCancellationRequest::NotFound);
        }
        let status = run
            .try_get::<String, _>("status")
            .map_err(|error| IronCrewError::Validation(format!("Column error: {error}")))?
            .parse::<RunStatus>()?;
        if status.is_terminal() {
            self.delete_human_inputs_for_run(&mut tx, run_id).await?;
            tx.commit().await.map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL terminal cancellation lookup commit failed: {error}"
                ))
            })?;
            return Ok(RunCancellationRequest::Terminal(status));
        }
        let run_owner: String = run
            .try_get("owner_instance_id")
            .map_err(|error| IronCrewError::Validation(format!("Column error: {error}")))?;

        let key_sql = format!(
            "SELECT key_hash FROM {} \
             WHERE operation = $1 AND scope = $2 AND resource_id = $3 \
               AND state IN ('claimed', 'running') \
             ORDER BY created_at DESC LIMIT 2",
            self.idempotency_table
        );
        let keys: Vec<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(key_sql))
            .bind(RUN_OPERATION)
            .bind(flow)
            .bind(run_id)
            .fetch_all(&mut *tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL cancellation ledger lookup failed: {error}"
                ))
            })?;
        let [key_hash] = keys.as_slice() else {
            if keys.len() > 1 {
                return Err(IronCrewError::Conflict(format!(
                    "Run '{run_id}' has multiple active idempotency ledgers"
                )));
            }
            tx.commit().await.map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL non-durable cancellation lookup commit failed: {error}"
                ))
            })?;
            return Ok(RunCancellationRequest::NotDurable);
        };

        self.lock_idempotency_key(&mut tx, key_hash).await?;
        let ledger_sql = format!(
            "SELECT owner_instance_id, state, cancel_requested_at FROM {} \
             WHERE key_hash = $1 FOR UPDATE",
            self.idempotency_table
        );
        let Some(ledger) = sqlx::query(sqlx::AssertSqlSafe(ledger_sql))
            .bind(key_hash)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL cancellation ledger fence failed: {error}"
                ))
            })?
        else {
            tx.rollback().await.map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL vanished cancellation ledger rollback failed: {error}"
                ))
            })?;
            return Ok(RunCancellationRequest::NotDurable);
        };
        let ledger_owner: String = ledger
            .try_get("owner_instance_id")
            .map_err(|error| IronCrewError::Validation(format!("Column error: {error}")))?;
        let ledger_state = ledger
            .try_get::<String, _>("state")
            .map_err(|error| IronCrewError::Validation(format!("Column error: {error}")))?
            .parse::<IdempotencyState>()?;
        if ledger_owner != run_owner || !ledger_state.is_in_flight() {
            tx.rollback().await.map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL changed cancellation fence rollback failed: {error}"
                ))
            })?;
            return Ok(RunCancellationRequest::NotDurable);
        }
        let already_requested = ledger
            .try_get::<Option<String>, _>("cancel_requested_at")
            .map_err(|error| IronCrewError::Validation(format!("Column error: {error}")))?
            .is_some();
        let (database_now, _) = self
            .database_clock_with_deadline(&mut tx, 0, "run cancellation request")
            .await?;
        let update_sql = format!(
            "UPDATE {} SET \
                 cancel_requested_at = COALESCE(cancel_requested_at, $1), \
                 updated_at = CASE WHEN cancel_requested_at IS NULL THEN $1 ELSE updated_at END \
             WHERE key_hash = $2 AND owner_instance_id = $3 \
               AND state IN ('claimed', 'running')",
            self.idempotency_table
        );
        let changed = sqlx::query(sqlx::AssertSqlSafe(update_sql))
            .bind(&database_now)
            .bind(key_hash)
            .bind(&run_owner)
            .execute(&mut *tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL cancellation request failed: {error}"
                ))
            })?;
        if changed.rows_affected() != 1 {
            tx.rollback().await.map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL cancellation race rollback failed: {error}"
                ))
            })?;
            return Ok(RunCancellationRequest::NotDurable);
        }
        self.delete_human_inputs_for_run(&mut tx, run_id).await?;
        tx.commit().await.map_err(|error| {
            IronCrewError::Validation(format!(
                "PostgreSQL cancellation request commit failed: {error}"
            ))
        })?;
        Ok(RunCancellationRequest::Requested {
            owner_instance_id: run_owner,
            already_requested,
        })
    }

    async fn register_human_input(
        &self,
        registration: &DurableHumanInputRegistration,
    ) -> Result<HumanInputRegistrationOutcome> {
        registration.validate()?;
        let Some(keyring) = self.human_input_keyring.as_ref() else {
            return Ok(HumanInputRegistrationOutcome::NotDurable);
        };
        let aad = registration.aad(self.lease.instance_id())?;
        let encrypted = keyring.seal_question(&aad, &registration.question)?;

        let mut tx = self.pool.begin().await.map_err(|error| {
            IronCrewError::Validation(format!(
                "PostgreSQL human-input registration transaction failed: {error}"
            ))
        })?;
        self.lock_run_fence(&mut tx, true).await?;
        self.lock_resource(&mut tx, RUN_OPERATION, "", &registration.run_id)
            .await?;
        self.lock_idempotency_key(&mut tx, &registration.key_hash)
            .await?;
        let (database_now, expires_at) = self
            .database_clock_with_deadline(
                &mut tx,
                registration.question.timeout_s,
                "human-input registration",
            )
            .await?;

        let fence_sql = format!(
            "SELECT EXISTS (\
                 SELECT 1 FROM {runs} AS run \
                 JOIN {idempotency} AS idem \
                   ON idem.operation = $1 AND idem.scope = run.flow \
                  AND idem.resource_id = run.run_id \
                 WHERE run.run_id = $2 AND run.flow = $3 \
                   AND run.owner_instance_id = $4 \
                   AND run.status IN ('running', 'waiting_for_input') \
                   AND run.lease_expires_at <> '' \
                   AND run.lease_expires_at::timestamptz > $5::timestamptz \
                   AND idem.key_hash = $6 AND idem.attempt_id = $7 \
                   AND idem.owner_instance_id = run.owner_instance_id \
                   AND idem.state = 'running' \
                   AND idem.lease_expires_at <> '' \
                   AND idem.lease_expires_at::timestamptz > $5::timestamptz\
             )",
            runs = self.table_name,
            idempotency = self.idempotency_table,
        );
        let owns_fence: bool = sqlx::query_scalar(sqlx::AssertSqlSafe(fence_sql))
            .bind(RUN_OPERATION)
            .bind(&registration.run_id)
            .bind(&registration.flow)
            .bind(self.lease.instance_id())
            .bind(&database_now)
            .bind(&registration.key_hash)
            .bind(&registration.attempt_id)
            .fetch_one(&mut *tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL human-input registration fence lookup failed: {error}"
                ))
            })?;
        if !owns_fence {
            tx.rollback().await.map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL human-input registration rollback failed: {error}"
                ))
            })?;
            return Err(IronCrewError::Conflict(format!(
                "Run '{}' no longer owns the active keyed attempt for this human-input question",
                registration.run_id
            )));
        }

        // Resolve an idempotent retry before applying capacity so retrying an
        // already-retained question never consumes (or appears to consume) a
        // second slot. The per-run resource advisory lock serializes all
        // conforming registrations for exact aggregate admission.
        let existing_sql = format!(
            "SELECT flow, owner_instance_id, key_hash, attempt_id, \
                    question_digest, state, expires_at > $3::timestamptz \
             FROM {} WHERE run_id = $1 AND question_id = $2 FOR UPDATE",
            self.human_inputs_table
        );
        let existing: Option<(String, String, String, String, String, String, bool)> =
            sqlx::query_as(sqlx::AssertSqlSafe(existing_sql))
                .bind(&registration.run_id)
                .bind(&registration.question.question_id)
                .bind(&database_now)
                .fetch_optional(&mut *tx)
                .await
                .map_err(|error| {
                    IronCrewError::Validation(format!(
                        "PostgreSQL human-input idempotent registration lookup failed: {error}"
                    ))
                })?;
        if let Some((flow, owner, key_hash, attempt_id, digest, state, unexpired)) = existing {
            if flow == registration.flow
                && owner == self.lease.instance_id()
                && key_hash == registration.key_hash
                && attempt_id == registration.attempt_id
                && digest == aad.question_digest
                && state == "pending"
                && unexpired
            {
                tx.commit().await.map_err(|error| {
                    IronCrewError::Validation(format!(
                        "PostgreSQL idempotent human-input registration commit failed: {error}"
                    ))
                })?;
                return Ok(HumanInputRegistrationOutcome::Registered);
            }
            return Err(IronCrewError::Conflict(format!(
                "Human-input question '{}' is already registered under another run attempt",
                registration.question.question_id
            )));
        }

        let capacity_sql = format!(
            "SELECT COUNT(*)::BIGINT, \
                    COALESCE(SUM(octet_length(question_nonce) + \
                                 octet_length(question_ciphertext)), 0)::BIGINT \
             FROM {} WHERE run_id = $1 AND flow = $2 AND state = 'pending' \
               AND expires_at > $3::timestamptz",
            self.human_inputs_table
        );
        let (pending_rows, pending_ciphertext_bytes): (i64, i64) =
            sqlx::query_as(sqlx::AssertSqlSafe(capacity_sql))
                .bind(&registration.run_id)
                .bind(&registration.flow)
                .bind(&database_now)
                .fetch_one(&mut *tx)
                .await
                .map_err(|error| {
                    IronCrewError::Validation(format!(
                        "PostgreSQL human-input registration capacity lookup failed: {error}"
                    ))
                })?;
        let pending_rows = usize::try_from(pending_rows).map_err(|_| {
            IronCrewError::Validation("PostgreSQL human-input row accounting is invalid".into())
        })?;
        let pending_ciphertext_bytes = usize::try_from(pending_ciphertext_bytes).map_err(|_| {
            IronCrewError::Validation(
                "PostgreSQL human-input ciphertext accounting is invalid".into(),
            )
        })?;
        let incoming_ciphertext_bytes = encrypted
            .nonce
            .len()
            .checked_add(encrypted.ciphertext.len())
            .ok_or_else(|| {
                IronCrewError::Validation(
                    "PostgreSQL human-input ciphertext byte count overflow".into(),
                )
            })?;
        let projected_ciphertext_bytes = pending_ciphertext_bytes
            .checked_add(incoming_ciphertext_bytes)
            .ok_or_else(|| {
                IronCrewError::Validation(
                    "PostgreSQL human-input ciphertext accounting overflow".into(),
                )
            })?;
        if pending_rows >= self.human_input_max_pending_rows
            || projected_ciphertext_bytes > self.human_input_max_pending_ciphertext_bytes
        {
            return Err(IronCrewError::Conflict(format!(
                "PostgreSQL human-input mailbox reached its configured capacity ({} rows, {} ciphertext bytes)",
                self.human_input_max_pending_rows, self.human_input_max_pending_ciphertext_bytes,
            )));
        }

        let insert_sql = format!(
            "INSERT INTO {} (\
                 run_id, question_id, flow, owner_instance_id, key_hash, attempt_id, \
                 question_digest, question_key_fingerprint, question_nonce, question_ciphertext, \
                 state, created_at, expires_at\
             ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, 'pending', \
                       $11::timestamptz, $12::timestamptz) \
             ON CONFLICT (run_id, question_id) DO NOTHING",
            self.human_inputs_table
        );
        let inserted = sqlx::query(sqlx::AssertSqlSafe(insert_sql))
            .bind(&registration.run_id)
            .bind(&registration.question.question_id)
            .bind(&registration.flow)
            .bind(self.lease.instance_id())
            .bind(&registration.key_hash)
            .bind(&registration.attempt_id)
            .bind(&aad.question_digest)
            .bind(&encrypted.key_fingerprint)
            .bind(&encrypted.nonce)
            .bind(&encrypted.ciphertext)
            .bind(&database_now)
            .bind(&expires_at)
            .execute(&mut *tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL human-input registration insert failed: {error}"
                ))
            })?;
        if inserted.rows_affected() == 0 {
            let existing_sql = format!(
                "SELECT EXISTS (SELECT 1 FROM {} \
                     WHERE run_id = $1 AND question_id = $2 AND flow = $3 \
                       AND owner_instance_id = $4 AND key_hash = $5 AND attempt_id = $6 \
                       AND question_digest = $7 AND state = 'pending' \
                       AND expires_at > $8::timestamptz)",
                self.human_inputs_table
            );
            let same_fence: bool = sqlx::query_scalar(sqlx::AssertSqlSafe(existing_sql))
                .bind(&registration.run_id)
                .bind(&registration.question.question_id)
                .bind(&registration.flow)
                .bind(self.lease.instance_id())
                .bind(&registration.key_hash)
                .bind(&registration.attempt_id)
                .bind(&aad.question_digest)
                .bind(&database_now)
                .fetch_one(&mut *tx)
                .await
                .map_err(|error| {
                    IronCrewError::Validation(format!(
                        "PostgreSQL human-input registration collision lookup failed: {error}"
                    ))
                })?;
            if !same_fence {
                tx.rollback().await.map_err(|error| {
                    IronCrewError::Validation(format!(
                        "PostgreSQL human-input collision rollback failed: {error}"
                    ))
                })?;
                return Err(IronCrewError::Conflict(format!(
                    "Human-input question '{}' is already registered under another run attempt",
                    registration.question.question_id
                )));
            }
        }
        tx.commit().await.map_err(|error| {
            IronCrewError::Validation(format!(
                "PostgreSQL human-input registration commit failed: {error}"
            ))
        })?;
        Ok(HumanInputRegistrationOutcome::Registered)
    }

    async fn list_human_inputs(&self, flow: &str, run_id: &str) -> Result<HumanInputListOutcome> {
        validate_human_input_route("flow", flow, 255)?;
        validate_human_input_route("run id", run_id, 128)?;
        let Some(keyring) = self.human_input_keyring.as_ref() else {
            return Ok(HumanInputListOutcome::NotDurable);
        };
        let _read_permit = self.human_input_read_slots.try_acquire().map_err(|_| {
            IronCrewError::Conflict(format!(
                "PostgreSQL human-input read concurrency is exhausted; raise \
                 {HUMAN_INPUT_READ_CONCURRENCY_ENV} if the pod has sufficient memory"
            ))
        })?;

        let owner_sql = format!(
            "SELECT run.owner_instance_id FROM {runs} AS run \
             JOIN {idempotency} AS idem \
               ON idem.operation = $1 AND idem.scope = run.flow \
              AND idem.resource_id = run.run_id \
              AND idem.owner_instance_id = run.owner_instance_id \
             WHERE run.run_id = $2 AND run.flow = $3 \
               AND run.status IN ('running', 'waiting_for_input') \
               AND run.lease_expires_at <> '' \
               AND run.lease_expires_at::timestamptz > clock_timestamp() \
               AND idem.state = 'running' \
               AND idem.lease_expires_at <> '' \
               AND idem.lease_expires_at::timestamptz > clock_timestamp() \
             ORDER BY idem.created_at DESC LIMIT 2",
            runs = self.table_name,
            idempotency = self.idempotency_table,
        );
        let owners: Vec<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(owner_sql))
            .bind(RUN_OPERATION)
            .bind(run_id)
            .bind(flow)
            .fetch_all(&self.pool)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL human-input owner lookup failed: {error}"
                ))
            })?;
        let [owner_instance_id] = owners.as_slice() else {
            if owners.len() > 1 {
                return Err(IronCrewError::Conflict(format!(
                    "Run '{run_id}' has multiple active keyed attempts"
                )));
            }
            return Ok(HumanInputListOutcome::NotDurable);
        };

        // Size metadata first so a corrupted or externally-written mailbox
        // cannot force the process to materialize an unbounded encrypted row
        // set. The subsequent query still fetches one sentinel row and
        // re-checks cumulative bytes to fail closed across concurrent inserts.
        let list_limits_sql = format!(
            "SELECT COUNT(*)::BIGINT, \
                    COALESCE(SUM(octet_length(human.question_nonce) + \
                                 octet_length(human.question_ciphertext)), 0)::BIGINT \
             FROM {human_inputs} AS human \
             JOIN {runs} AS run ON run.run_id = human.run_id \
             JOIN {idempotency} AS idem \
               ON idem.key_hash = human.key_hash \
              AND idem.attempt_id = human.attempt_id \
              AND idem.owner_instance_id = human.owner_instance_id \
              AND idem.operation = $1 AND idem.scope = human.flow \
              AND idem.resource_id = human.run_id \
             WHERE human.run_id = $2 AND human.flow = $3 \
               AND human.state = 'pending' \
               AND human.expires_at > clock_timestamp() \
               AND run.status IN ('running', 'waiting_for_input') \
               AND run.owner_instance_id = human.owner_instance_id \
               AND run.lease_expires_at <> '' \
               AND run.lease_expires_at::timestamptz > clock_timestamp() \
               AND idem.state = 'running' \
               AND idem.lease_expires_at <> '' \
               AND idem.lease_expires_at::timestamptz > clock_timestamp()",
            human_inputs = self.human_inputs_table,
            runs = self.table_name,
            idempotency = self.idempotency_table,
        );
        let (pending_rows, pending_ciphertext_bytes): (i64, i64) =
            sqlx::query_as(sqlx::AssertSqlSafe(list_limits_sql))
                .bind(RUN_OPERATION)
                .bind(run_id)
                .bind(flow)
                .fetch_one(&self.pool)
                .await
                .map_err(|error| {
                    IronCrewError::Validation(format!(
                        "PostgreSQL human-input list bounds failed: {error}"
                    ))
                })?;
        let pending_rows = usize::try_from(pending_rows).map_err(|_| {
            IronCrewError::Validation("PostgreSQL human-input pending row count is invalid".into())
        })?;
        let pending_ciphertext_bytes = usize::try_from(pending_ciphertext_bytes).map_err(|_| {
            IronCrewError::Validation(
                "PostgreSQL human-input ciphertext accounting is invalid".into(),
            )
        })?;
        if pending_rows > self.human_input_max_pending_rows
            || pending_ciphertext_bytes > self.human_input_max_pending_ciphertext_bytes
        {
            return Err(IronCrewError::Validation(format!(
                "PostgreSQL human-input mailbox exceeds its configured read bounds ({} rows, {} ciphertext bytes)",
                self.human_input_max_pending_rows, self.human_input_max_pending_ciphertext_bytes,
            )));
        }

        let list_sql = format!(
            "SELECT human.question_id, human.owner_instance_id, human.key_hash, \
                    human.attempt_id, human.question_digest, human.question_key_fingerprint, \
                    human.question_nonce, human.question_ciphertext \
             FROM {human_inputs} AS human \
             JOIN {runs} AS run ON run.run_id = human.run_id \
             JOIN {idempotency} AS idem \
               ON idem.key_hash = human.key_hash \
              AND idem.attempt_id = human.attempt_id \
              AND idem.owner_instance_id = human.owner_instance_id \
              AND idem.operation = $1 AND idem.scope = human.flow \
              AND idem.resource_id = human.run_id \
             WHERE human.run_id = $2 AND human.flow = $3 \
               AND human.state = 'pending' \
               AND human.expires_at > clock_timestamp() \
               AND run.status IN ('running', 'waiting_for_input') \
               AND run.owner_instance_id = human.owner_instance_id \
               AND run.lease_expires_at <> '' \
               AND run.lease_expires_at::timestamptz > clock_timestamp() \
               AND idem.state = 'running' \
               AND idem.lease_expires_at <> '' \
               AND idem.lease_expires_at::timestamptz > clock_timestamp() \
             ORDER BY human.created_at, human.question_id LIMIT {}",
            self.human_input_max_pending_rows + 1,
            human_inputs = self.human_inputs_table,
            runs = self.table_name,
            idempotency = self.idempotency_table,
        );
        let rows = sqlx::query(sqlx::AssertSqlSafe(list_sql))
            .bind(RUN_OPERATION)
            .bind(run_id)
            .bind(flow)
            .fetch_all(&self.pool)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!("PostgreSQL human-input list failed: {error}"))
            })?;
        if rows.len() > self.human_input_max_pending_rows {
            return Err(IronCrewError::Validation(format!(
                "PostgreSQL human-input mailbox exceeds its {}-row configured limit",
                self.human_input_max_pending_rows,
            )));
        }
        let mut questions = Vec::with_capacity(rows.len());
        let mut read_ciphertext_bytes = 0usize;
        for row in rows {
            let question_id: String = row
                .try_get("question_id")
                .map_err(|error| IronCrewError::Validation(format!("Column error: {error}")))?;
            let row_owner: String = row
                .try_get("owner_instance_id")
                .map_err(|error| IronCrewError::Validation(format!("Column error: {error}")))?;
            let key_hash: String = row
                .try_get("key_hash")
                .map_err(|error| IronCrewError::Validation(format!("Column error: {error}")))?;
            let attempt_id: String = row
                .try_get("attempt_id")
                .map_err(|error| IronCrewError::Validation(format!("Column error: {error}")))?;
            let question_digest: String = row
                .try_get("question_digest")
                .map_err(|error| IronCrewError::Validation(format!("Column error: {error}")))?;
            let fingerprint: String = row
                .try_get("question_key_fingerprint")
                .map_err(|error| IronCrewError::Validation(format!("Column error: {error}")))?;
            let nonce: Vec<u8> = row
                .try_get("question_nonce")
                .map_err(|error| IronCrewError::Validation(format!("Column error: {error}")))?;
            let ciphertext: Vec<u8> = row
                .try_get("question_ciphertext")
                .map_err(|error| IronCrewError::Validation(format!("Column error: {error}")))?;
            read_ciphertext_bytes = read_ciphertext_bytes
                .checked_add(nonce.len())
                .and_then(|bytes| bytes.checked_add(ciphertext.len()))
                .ok_or_else(|| {
                    IronCrewError::Validation(
                        "PostgreSQL human-input ciphertext byte count overflow".into(),
                    )
                })?;
            if read_ciphertext_bytes > self.human_input_max_pending_ciphertext_bytes {
                return Err(IronCrewError::Validation(format!(
                    "PostgreSQL human-input mailbox exceeds its {}-byte configured ciphertext limit",
                    self.human_input_max_pending_ciphertext_bytes,
                )));
            }
            let aad = HumanInputAad::new(
                flow,
                run_id,
                &question_id,
                &question_digest,
                &row_owner,
                &key_hash,
                &attempt_id,
            )?;
            let info = keyring.open_question(&aad, &fingerprint, &nonce, &ciphertext)?;
            if info.question_id != question_id || row_owner != *owner_instance_id {
                return Err(IronCrewError::Conflict(
                    "Durable human-input question metadata does not match its routing fence".into(),
                ));
            }
            let question = DurableHumanInputQuestion {
                info,
                owner_instance_id: row_owner,
            };
            question.validate()?;
            questions.push(question);
        }
        Ok(HumanInputListOutcome::Shared {
            owner_instance_id: owner_instance_id.clone(),
            questions,
        })
    }

    async fn answer_human_input(
        &self,
        flow: &str,
        run_id: &str,
        question_id: &str,
        answer: &serde_json::Value,
    ) -> Result<HumanInputAnswerOutcome> {
        validate_human_input_route("flow", flow, 255)?;
        validate_human_input_route("run id", run_id, 128)?;
        validate_human_input_route("question id", question_id, 128)?;
        validate_durable_answer(answer)?;
        let Some(keyring) = self.human_input_keyring.as_ref() else {
            return Ok(HumanInputAnswerOutcome::NotDurable);
        };

        let mut tx = self.pool.begin().await.map_err(|error| {
            IronCrewError::Validation(format!(
                "PostgreSQL human-input answer transaction failed: {error}"
            ))
        })?;
        self.lock_run_fence(&mut tx, true).await?;
        self.lock_resource(&mut tx, RUN_OPERATION, "", run_id)
            .await?;
        let row_sql = format!(
            "SELECT owner_instance_id, key_hash, attempt_id, question_digest, state \
             FROM {} WHERE run_id = $1 AND question_id = $2 AND flow = $3 \
             FOR UPDATE",
            self.human_inputs_table
        );
        let Some(row) = sqlx::query(sqlx::AssertSqlSafe(row_sql))
            .bind(run_id)
            .bind(question_id)
            .bind(flow)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL human-input answer lookup failed: {error}"
                ))
            })?
        else {
            tx.commit().await.map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL missing human-input answer commit failed: {error}"
                ))
            })?;
            return Ok(HumanInputAnswerOutcome::NotFound);
        };
        let owner_instance_id: String = row
            .try_get("owner_instance_id")
            .map_err(|error| IronCrewError::Validation(format!("Column error: {error}")))?;
        let key_hash: String = row
            .try_get("key_hash")
            .map_err(|error| IronCrewError::Validation(format!("Column error: {error}")))?;
        let attempt_id: String = row
            .try_get("attempt_id")
            .map_err(|error| IronCrewError::Validation(format!("Column error: {error}")))?;
        let question_digest: String = row
            .try_get("question_digest")
            .map_err(|error| IronCrewError::Validation(format!("Column error: {error}")))?;
        let state: String = row
            .try_get("state")
            .map_err(|error| IronCrewError::Validation(format!("Column error: {error}")))?;
        self.lock_idempotency_key(&mut tx, &key_hash).await?;
        let (database_now, _) = self
            .database_clock_with_deadline(&mut tx, 0, "human-input answer")
            .await?;
        let active_sql = format!(
            "SELECT EXISTS (\
                 SELECT 1 FROM {human_inputs} AS human \
                 JOIN {runs} AS run ON run.run_id = human.run_id \
                 JOIN {idempotency} AS idem \
                   ON idem.key_hash = human.key_hash \
                  AND idem.attempt_id = human.attempt_id \
                  AND idem.owner_instance_id = human.owner_instance_id \
                  AND idem.operation = $1 AND idem.scope = human.flow \
                  AND idem.resource_id = human.run_id \
                 WHERE human.run_id = $2 AND human.question_id = $3 \
                   AND human.flow = $4 \
                   AND (human.state = 'answered' OR human.expires_at > $5::timestamptz) \
                   AND run.status IN ('running', 'waiting_for_input') \
                   AND run.owner_instance_id = human.owner_instance_id \
                   AND run.lease_expires_at <> '' \
                   AND run.lease_expires_at::timestamptz > $5::timestamptz \
                   AND idem.state = 'running' \
                   AND idem.lease_expires_at <> '' \
                   AND idem.lease_expires_at::timestamptz > $5::timestamptz\
             )",
            human_inputs = self.human_inputs_table,
            runs = self.table_name,
            idempotency = self.idempotency_table,
        );
        let active: bool = sqlx::query_scalar(sqlx::AssertSqlSafe(active_sql))
            .bind(RUN_OPERATION)
            .bind(run_id)
            .bind(question_id)
            .bind(flow)
            .bind(&database_now)
            .fetch_one(&mut *tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL human-input answer fence check failed: {error}"
                ))
            })?;
        if !active {
            let delete_sql = format!(
                "DELETE FROM {} WHERE run_id = $1 AND question_id = $2",
                self.human_inputs_table
            );
            sqlx::query(sqlx::AssertSqlSafe(delete_sql))
                .bind(run_id)
                .bind(question_id)
                .execute(&mut *tx)
                .await
                .map_err(|error| {
                    IronCrewError::Validation(format!(
                        "PostgreSQL stale human-input cleanup failed: {error}"
                    ))
                })?;
            tx.commit().await.map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL stale human-input answer commit failed: {error}"
                ))
            })?;
            return Ok(HumanInputAnswerOutcome::NotFound);
        }
        if state == "answered" {
            tx.commit().await.map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL duplicate human-input answer commit failed: {error}"
                ))
            })?;
            return Ok(HumanInputAnswerOutcome::AlreadyAnswered);
        }
        if state != "pending" {
            return Err(IronCrewError::Validation(format!(
                "PostgreSQL human-input row has invalid state '{state}'"
            )));
        }

        let aad = HumanInputAad::new(
            flow,
            run_id,
            question_id,
            &question_digest,
            &owner_instance_id,
            &key_hash,
            &attempt_id,
        )?;
        let encrypted = keyring.seal_json(&aad, answer)?;
        let update_sql = format!(
            "UPDATE {human_inputs} AS human SET \
                 answer_key_fingerprint = $1, answer_nonce = $2, answer_ciphertext = $3, \
                 state = 'answered', answered_at = $4::timestamptz \
             WHERE human.run_id = $5 AND human.question_id = $6 AND human.flow = $7 \
               AND human.owner_instance_id = $8 AND human.key_hash = $9 \
               AND human.attempt_id = $10 AND human.question_digest = $11 \
               AND human.state = 'pending' \
               AND human.expires_at > $4::timestamptz \
               AND EXISTS (SELECT 1 FROM {runs} AS run \
                   WHERE run.run_id = human.run_id \
                     AND run.owner_instance_id = human.owner_instance_id \
                     AND run.status IN ('running', 'waiting_for_input') \
                     AND run.lease_expires_at <> '' \
                     AND run.lease_expires_at::timestamptz > $4::timestamptz) \
               AND EXISTS (SELECT 1 FROM {idempotency} AS idem \
                   WHERE idem.key_hash = human.key_hash \
                     AND idem.attempt_id = human.attempt_id \
                     AND idem.owner_instance_id = human.owner_instance_id \
                     AND idem.operation = $12 AND idem.scope = human.flow \
                     AND idem.resource_id = human.run_id AND idem.state = 'running' \
                     AND idem.lease_expires_at <> '' \
                     AND idem.lease_expires_at::timestamptz > $4::timestamptz)",
            human_inputs = self.human_inputs_table,
            runs = self.table_name,
            idempotency = self.idempotency_table,
        );
        let updated = sqlx::query(sqlx::AssertSqlSafe(update_sql))
            .bind(&encrypted.key_fingerprint)
            .bind(&encrypted.nonce)
            .bind(&encrypted.ciphertext)
            .bind(&database_now)
            .bind(run_id)
            .bind(question_id)
            .bind(flow)
            .bind(&owner_instance_id)
            .bind(&key_hash)
            .bind(&attempt_id)
            .bind(&question_digest)
            .bind(RUN_OPERATION)
            .execute(&mut *tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL human-input answer update failed: {error}"
                ))
            })?;
        if updated.rows_affected() != 1 {
            tx.rollback().await.map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL human-input answer race rollback failed: {error}"
                ))
            })?;
            return Ok(HumanInputAnswerOutcome::NotFound);
        }
        tx.commit().await.map_err(|error| {
            IronCrewError::Validation(format!(
                "PostgreSQL human-input answer commit failed: {error}"
            ))
        })?;
        Ok(HumanInputAnswerOutcome::Queued { owner_instance_id })
    }

    async fn read_human_input(
        &self,
        registration: &DurableHumanInputRegistration,
    ) -> Result<HumanInputReadOutcome> {
        registration.validate()?;
        let Some(keyring) = self.human_input_keyring.as_ref() else {
            return Ok(HumanInputReadOutcome::NotDurable);
        };
        let aad = registration.aad(self.lease.instance_id())?;
        let sql = format!(
            "SELECT human.state, human.answer_key_fingerprint, human.answer_nonce, \
                    human.answer_ciphertext, human.question_digest \
             FROM {human_inputs} AS human \
             JOIN {runs} AS run ON run.run_id = human.run_id \
             JOIN {idempotency} AS idem \
               ON idem.key_hash = human.key_hash \
              AND idem.attempt_id = human.attempt_id \
              AND idem.owner_instance_id = human.owner_instance_id \
              AND idem.operation = $1 AND idem.scope = human.flow \
              AND idem.resource_id = human.run_id \
             WHERE human.run_id = $2 AND human.question_id = $3 \
               AND human.flow = $4 AND human.owner_instance_id = $5 \
               AND human.key_hash = $6 AND human.attempt_id = $7 \
               AND human.question_digest = $8 \
               AND (human.state = 'answered' OR human.expires_at > clock_timestamp()) \
               AND run.owner_instance_id = human.owner_instance_id \
               AND run.status IN ('running', 'waiting_for_input') \
               AND run.lease_expires_at <> '' \
               AND run.lease_expires_at::timestamptz > clock_timestamp() \
               AND idem.state = 'running' \
               AND idem.lease_expires_at <> '' \
               AND idem.lease_expires_at::timestamptz > clock_timestamp()",
            human_inputs = self.human_inputs_table,
            runs = self.table_name,
            idempotency = self.idempotency_table,
        );
        let Some(row) = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(RUN_OPERATION)
            .bind(&registration.run_id)
            .bind(&registration.question.question_id)
            .bind(&registration.flow)
            .bind(self.lease.instance_id())
            .bind(&registration.key_hash)
            .bind(&registration.attempt_id)
            .bind(&aad.question_digest)
            .fetch_optional(&self.pool)
            .await
            .map_err(|error| {
                IronCrewError::Io(std::io::Error::other(format!(
                    "PostgreSQL human-input owner read failed: {error}"
                )))
            })?
        else {
            return Ok(HumanInputReadOutcome::NotFound);
        };
        let question_digest: String = row
            .try_get("question_digest")
            .map_err(|error| IronCrewError::Validation(format!("Column error: {error}")))?;
        if question_digest != aad.question_digest {
            return Err(IronCrewError::Conflict(
                "Durable human-input question digest does not match its registration".into(),
            ));
        }
        let state: String = row
            .try_get("state")
            .map_err(|error| IronCrewError::Validation(format!("Column error: {error}")))?;
        if state == "pending" {
            return Ok(HumanInputReadOutcome::Pending);
        }
        if state != "answered" {
            return Err(IronCrewError::Validation(format!(
                "PostgreSQL human-input row has invalid state '{state}'"
            )));
        }
        let fingerprint: String = row
            .try_get::<Option<String>, _>("answer_key_fingerprint")
            .map_err(|error| IronCrewError::Validation(format!("Column error: {error}")))?
            .ok_or_else(|| {
                IronCrewError::Validation(
                    "Answered PostgreSQL human-input row has no key fingerprint".into(),
                )
            })?;
        let nonce: Vec<u8> = row
            .try_get::<Option<Vec<u8>>, _>("answer_nonce")
            .map_err(|error| IronCrewError::Validation(format!("Column error: {error}")))?
            .ok_or_else(|| {
                IronCrewError::Validation("Answered PostgreSQL human-input row has no nonce".into())
            })?;
        let ciphertext: Vec<u8> = row
            .try_get::<Option<Vec<u8>>, _>("answer_ciphertext")
            .map_err(|error| IronCrewError::Validation(format!("Column error: {error}")))?
            .ok_or_else(|| {
                IronCrewError::Validation(
                    "Answered PostgreSQL human-input row has no ciphertext".into(),
                )
            })?;
        let answer = keyring.open_json(&aad, &fingerprint, &nonce, &ciphertext)?;
        Ok(HumanInputReadOutcome::Answered(answer))
    }

    async fn close_human_input(
        &self,
        registration: &DurableHumanInputRegistration,
    ) -> Result<bool> {
        registration.validate()?;
        if self.human_input_keyring.is_none() {
            return Ok(false);
        }
        let mut tx = self.pool.begin().await.map_err(|error| {
            IronCrewError::Validation(format!(
                "PostgreSQL human-input close transaction failed: {error}"
            ))
        })?;
        self.lock_run_fence(&mut tx, true).await?;
        self.lock_resource(&mut tx, RUN_OPERATION, "", &registration.run_id)
            .await?;
        let expected_question_digest = question_digest(&registration.question)?;
        let sql = format!(
            "DELETE FROM {} WHERE run_id = $1 AND question_id = $2 AND flow = $3 \
               AND owner_instance_id = $4 AND key_hash = $5 AND attempt_id = $6 \
               AND question_digest = $7",
            self.human_inputs_table
        );
        let deleted = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(&registration.run_id)
            .bind(&registration.question.question_id)
            .bind(&registration.flow)
            .bind(self.lease.instance_id())
            .bind(&registration.key_hash)
            .bind(&registration.attempt_id)
            .bind(&expected_question_digest)
            .execute(&mut *tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!("PostgreSQL human-input close failed: {error}"))
            })?;
        tx.commit().await.map_err(|error| {
            IronCrewError::Validation(format!(
                "PostgreSQL human-input close commit failed: {error}"
            ))
        })?;
        Ok(deleted.rows_affected() == 1)
    }

    async fn complete_idempotency_with_limits(
        &self,
        completion: IdempotencyCompletion,
        limits: IdempotencyLimits,
    ) -> Result<IdempotencyCompletionOutcome> {
        completion.validate()?;
        limits.validate()?;
        let mut tx = self.pool.begin().await.map_err(|error| {
            IronCrewError::Validation(format!(
                "PostgreSQL idempotency completion transaction failed: {error}"
            ))
        })?;
        self.lock_idempotency_quota(&mut tx).await?;
        self.lock_idempotency_principal(&mut tx, &completion.principal_id)
            .await?;
        self.lock_idempotency_key(&mut tx, &completion.key_hash)
            .await?;
        let record = self
            .get_idempotency_in_transaction(&mut tx, &completion.key_hash)
            .await?
            .ok_or_else(|| {
                IronCrewError::Validation("Idempotency claim not found during completion".into())
            })?;
        if record.principal_id != completion.principal_id
            || record.request_fingerprint != completion.request_fingerprint
            || record.attempt_id != completion.attempt_id
            || record.owner_instance_id != completion.owner_instance_id
        {
            return Err(IronCrewError::Conflict(format!(
                "Idempotency claim '{}' is fenced by a different attempt",
                completion.key_hash
            )));
        }
        if record.state == IdempotencyState::Completed {
            let outcome = IdempotencyCompletionOutcome {
                replayable: record.replayable(),
                already_completed: true,
            };
            tx.commit().await.map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL idempotency completion commit failed: {error}"
                ))
            })?;
            return Ok(outcome);
        }
        if record.state == IdempotencyState::Indeterminate {
            return Err(IronCrewError::Conflict(
                "Indeterminate idempotency outcomes cannot be completed".into(),
            ));
        }
        let (database_completed_at, database_expires_at) = self
            .database_clock_with_deadline(&mut tx, record.ttl_seconds, "idempotency completion")
            .await?;

        let (global_usage, principal_usage) = self
            .idempotency_accounting_for_update(&mut tx, &completion.principal_id)
            .await?;
        let old_response_bytes = record.response_body.as_ref().map_or(0, String::len);
        let global_without_record = global_usage
            .response_bytes
            .checked_sub(old_response_bytes)
            .ok_or_else(|| {
                IronCrewError::Validation(
                    "PostgreSQL global idempotency response accounting is inconsistent".into(),
                )
            })?;
        let principal_without_record = principal_usage
            .response_bytes
            .checked_sub(old_response_bytes)
            .ok_or_else(|| {
                IronCrewError::Validation(
                    "PostgreSQL principal idempotency response accounting is inconsistent".into(),
                )
            })?;
        let response_body = completion.response_body.as_ref().filter(|body| {
            global_without_record
                .checked_add(body.len())
                .is_some_and(|total| total <= limits.global_max_response_bytes)
                && principal_without_record
                    .checked_add(body.len())
                    .is_some_and(|total| total <= limits.principal_max_response_bytes)
        });
        let sql = format!(
            "UPDATE {} SET state = 'completed', response_status = $1, \
             response_body = $2, lease_expires_at = '', updated_at = $3, \
             completed_at = $3, expires_at = $4 \
             WHERE key_hash = $5 AND request_fingerprint = $6 \
               AND attempt_id = $7 AND owner_instance_id = $8 \
               AND state IN ('claimed', 'running')",
            self.idempotency_table
        );
        let result = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(i32::from(completion.response_status))
            .bind(response_body)
            .bind(&database_completed_at)
            .bind(&database_expires_at)
            .bind(&completion.key_hash)
            .bind(&completion.request_fingerprint)
            .bind(&completion.attempt_id)
            .bind(&completion.owner_instance_id)
            .execute(&mut *tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL idempotency completion update failed: {error}"
                ))
            })?;
        if result.rows_affected() != 1 {
            return Err(IronCrewError::Conflict(format!(
                "Idempotency claim '{}' changed before completion",
                completion.key_hash
            )));
        }
        tx.commit().await.map_err(|error| {
            IronCrewError::Validation(format!(
                "PostgreSQL idempotency completion commit failed: {error}"
            ))
        })?;
        Ok(IdempotencyCompletionOutcome {
            replayable: response_body.is_some(),
            already_completed: false,
        })
    }

    async fn commit_conversation_idempotency_with_limits(
        &self,
        completion: IdempotencyCompletion,
        conversation: &ConversationRecord,
        limits: IdempotencyLimits,
    ) -> Result<ConversationIdempotencyCommit> {
        completion.validate()?;
        limits.validate()?;
        let messages_json = serde_json::to_string(&conversation.messages).map_err(|error| {
            IronCrewError::Validation(format!(
                "Failed to serialize idempotent conversation messages: {error}"
            ))
        })?;
        let expected_revision = i64::try_from(conversation.revision).map_err(|_| {
            IronCrewError::Validation("Conversation revision is out of range".into())
        })?;
        let mut tx = self.pool.begin().await.map_err(|error| {
            IronCrewError::Validation(format!(
                "PostgreSQL idempotent conversation transaction failed: {error}"
            ))
        })?;
        self.lock_idempotency_quota(&mut tx).await?;
        self.lock_idempotency_principal(&mut tx, &completion.principal_id)
            .await?;
        self.lock_resource(
            &mut tx,
            CONVERSATION_MESSAGE_OPERATION,
            conversation.flow_path.as_deref().unwrap_or(""),
            &conversation.id,
        )
        .await?;
        self.lock_idempotency_key(&mut tx, &completion.key_hash)
            .await?;
        let record = self
            .get_idempotency_in_transaction(&mut tx, &completion.key_hash)
            .await?
            .ok_or_else(|| {
                IronCrewError::Validation(
                    "Idempotency claim not found during conversation commit".into(),
                )
            })?;
        if record.principal_id != completion.principal_id
            || record.request_fingerprint != completion.request_fingerprint
            || record.attempt_id != completion.attempt_id
            || record.owner_instance_id != completion.owner_instance_id
        {
            return Err(IronCrewError::Conflict(format!(
                "Idempotency claim '{}' is fenced by a different attempt",
                completion.key_hash
            )));
        }
        if record.operation != CONVERSATION_MESSAGE_OPERATION
            || record.resource_id != conversation.id
            || record.scope != conversation.flow_path.as_deref().unwrap_or("")
        {
            return Err(IronCrewError::Conflict(format!(
                "Idempotency claim '{}' does not match the conversation scope",
                completion.key_hash
            )));
        }
        let base_revision = record.base_revision.ok_or_else(|| {
            IronCrewError::Validation("Conversation idempotency claim has no base revision".into())
        })?;
        if base_revision != conversation.revision {
            return Err(IronCrewError::Conflict(format!(
                "Conversation '{}' changed before idempotent commit",
                conversation.id
            )));
        }
        if record.state == IdempotencyState::Completed {
            tx.commit().await.map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL completed conversation commit failed: {error}"
                ))
            })?;
            return Ok(ConversationIdempotencyCommit {
                revision: base_revision.saturating_add(1),
                replayable: record.replayable(),
                already_completed: true,
            });
        }
        if record.state == IdempotencyState::Indeterminate {
            return Err(IronCrewError::Conflict(
                "Indeterminate conversation outcomes cannot be committed".into(),
            ));
        }
        let (database_completed_at, database_expires_at) = self
            .database_clock_with_deadline(
                &mut tx,
                record.ttl_seconds,
                "conversation idempotency completion",
            )
            .await?;

        let select_sql = format!(
            "SELECT revision FROM {} \
             WHERE id = $1 AND flow_path IS NOT DISTINCT FROM $2 FOR UPDATE",
            self.conversations_table
        );
        let current: Option<i64> = sqlx::query_scalar(sqlx::AssertSqlSafe(select_sql))
            .bind(&conversation.id)
            .bind(&conversation.flow_path)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL idempotent conversation revision read failed: {error}"
                ))
            })?;
        let revision: Option<i64> = match current {
            None if expected_revision == 0 => {
                let insert_sql = format!(
                    "INSERT INTO {} \
                     (id, flow_name, flow_path, agent_name, messages, created_at, updated_at, revision) \
                     VALUES ($1, $2, $3, $4, $5::jsonb, $6, $7, 1) \
                     ON CONFLICT (flow_path, id) DO NOTHING RETURNING revision",
                    self.conversations_table
                );
                sqlx::query_scalar(sqlx::AssertSqlSafe(insert_sql))
                    .bind(&conversation.id)
                    .bind(&conversation.flow_name)
                    .bind(&conversation.flow_path)
                    .bind(&conversation.agent_name)
                    .bind(&messages_json)
                    .bind(&conversation.created_at)
                    .bind(&conversation.updated_at)
                    .fetch_optional(&mut *tx)
                    .await
                    .map_err(|error| {
                        IronCrewError::Validation(format!(
                            "PostgreSQL idempotent conversation insert failed: {error}"
                        ))
                    })?
            }
            Some(current) if current == expected_revision => {
                let update_sql = format!(
                    "UPDATE {} SET flow_name = $3, agent_name = $4, \
                     messages = $5::jsonb, created_at = $6, updated_at = $7, \
                     revision = revision + 1 \
                     WHERE id = $1 AND flow_path IS NOT DISTINCT FROM $2 AND revision = $8 \
                     RETURNING revision",
                    self.conversations_table
                );
                sqlx::query_scalar(sqlx::AssertSqlSafe(update_sql))
                    .bind(&conversation.id)
                    .bind(&conversation.flow_path)
                    .bind(&conversation.flow_name)
                    .bind(&conversation.agent_name)
                    .bind(&messages_json)
                    .bind(&conversation.created_at)
                    .bind(&conversation.updated_at)
                    .bind(expected_revision)
                    .fetch_optional(&mut *tx)
                    .await
                    .map_err(|error| {
                        IronCrewError::Validation(format!(
                            "PostgreSQL idempotent conversation update failed: {error}"
                        ))
                    })?
            }
            _ => None,
        };
        let revision = revision.ok_or_else(|| {
            IronCrewError::Conflict(format!(
                "Conversation '{}' changed since revision {}; reopen it before saving",
                conversation.id, conversation.revision
            ))
        })?;

        let (global_usage, principal_usage) = self
            .idempotency_accounting_for_update(&mut tx, &completion.principal_id)
            .await?;
        let old_response_bytes = record.response_body.as_ref().map_or(0, String::len);
        let global_without_record = global_usage
            .response_bytes
            .checked_sub(old_response_bytes)
            .ok_or_else(|| {
                IronCrewError::Validation(
                    "PostgreSQL global idempotency response accounting is inconsistent".into(),
                )
            })?;
        let principal_without_record = principal_usage
            .response_bytes
            .checked_sub(old_response_bytes)
            .ok_or_else(|| {
                IronCrewError::Validation(
                    "PostgreSQL principal idempotency response accounting is inconsistent".into(),
                )
            })?;
        let response_body = completion.response_body.as_ref().filter(|body| {
            global_without_record
                .checked_add(body.len())
                .is_some_and(|total| total <= limits.global_max_response_bytes)
                && principal_without_record
                    .checked_add(body.len())
                    .is_some_and(|total| total <= limits.principal_max_response_bytes)
        });
        let update_idempotency = format!(
            "UPDATE {} SET state = 'completed', response_status = $1, \
             response_body = $2, lease_expires_at = '', updated_at = $3, \
             completed_at = $3, expires_at = $4 \
             WHERE key_hash = $5 AND request_fingerprint = $6 \
               AND attempt_id = $7 AND owner_instance_id = $8 \
               AND state IN ('claimed', 'running')",
            self.idempotency_table
        );
        let updated = sqlx::query(sqlx::AssertSqlSafe(update_idempotency))
            .bind(i32::from(completion.response_status))
            .bind(response_body)
            .bind(&database_completed_at)
            .bind(&database_expires_at)
            .bind(&completion.key_hash)
            .bind(&completion.request_fingerprint)
            .bind(&completion.attempt_id)
            .bind(&completion.owner_instance_id)
            .execute(&mut *tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL idempotent conversation completion failed: {error}"
                ))
            })?;
        if updated.rows_affected() != 1 {
            return Err(IronCrewError::Conflict(format!(
                "Idempotency claim '{}' changed before conversation commit",
                completion.key_hash
            )));
        }
        tx.commit().await.map_err(|error| {
            IronCrewError::Validation(format!(
                "PostgreSQL idempotent conversation commit failed: {error}"
            ))
        })?;
        Ok(ConversationIdempotencyCommit {
            revision: u64::try_from(revision)
                .map_err(|_| IronCrewError::Validation("Invalid conversation revision".into()))?,
            replayable: response_body.is_some(),
            already_completed: false,
        })
    }

    async fn mark_idempotency_indeterminate(
        &self,
        key_hash: &str,
        attempt_id: &str,
        completed_at: &str,
        expires_at: &str,
    ) -> Result<bool> {
        validate_digest("idempotency key hash", key_hash)?;
        parse_timestamp("idempotency completion time", completed_at)?;
        parse_timestamp("idempotency retention expiry", expires_at)?;
        let Some(principal_id) = self.idempotency_principal_for_key(key_hash).await? else {
            return Ok(false);
        };
        let mut tx = self.pool.begin().await.map_err(|error| {
            IronCrewError::Validation(format!(
                "PostgreSQL idempotency indeterminate transaction failed: {error}"
            ))
        })?;
        self.lock_idempotency_quota(&mut tx).await?;
        self.lock_idempotency_principal(&mut tx, &principal_id)
            .await?;
        self.lock_idempotency_key(&mut tx, key_hash).await?;
        let Some(record) = self
            .get_idempotency_in_transaction(&mut tx, key_hash)
            .await?
        else {
            return Ok(false);
        };
        if record.principal_id != principal_id {
            return Err(IronCrewError::Conflict(
                "Idempotency principal changed before indeterminate transition".into(),
            ));
        }
        if record.attempt_id != attempt_id {
            return Err(IronCrewError::Conflict(
                "Idempotency attempt changed before indeterminate transition".into(),
            ));
        }
        if record.state.is_terminal() {
            return Ok(false);
        }
        let (database_completed_at, database_expires_at) = self
            .database_clock_with_deadline(
                &mut tx,
                record.ttl_seconds,
                "idempotency indeterminate completion",
            )
            .await?;
        let sql = format!(
            "UPDATE {} SET state = 'indeterminate', response_status = NULL, \
             response_body = NULL, lease_expires_at = '', updated_at = $1, \
             completed_at = $1, expires_at = $2 \
             WHERE key_hash = $3 AND attempt_id = $4 \
               AND state IN ('claimed', 'running')",
            self.idempotency_table
        );
        let result = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(&database_completed_at)
            .bind(&database_expires_at)
            .bind(key_hash)
            .bind(attempt_id)
            .execute(&mut *tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL idempotency indeterminate update failed: {error}"
                ))
            })?;
        tx.commit().await.map_err(|error| {
            IronCrewError::Validation(format!(
                "PostgreSQL idempotency indeterminate commit failed: {error}"
            ))
        })?;
        Ok(result.rows_affected() == 1)
    }

    async fn release_idempotency(&self, key_hash: &str, attempt_id: &str) -> Result<bool> {
        validate_digest("idempotency key hash", key_hash)?;
        let Some(principal_id) = self.idempotency_principal_for_key(key_hash).await? else {
            return Ok(false);
        };
        let mut tx = self.pool.begin().await.map_err(|error| {
            IronCrewError::Validation(format!(
                "PostgreSQL idempotency release transaction failed: {error}"
            ))
        })?;
        self.lock_idempotency_quota(&mut tx).await?;
        self.lock_idempotency_principal(&mut tx, &principal_id)
            .await?;
        self.lock_idempotency_key(&mut tx, key_hash).await?;
        let Some(record) = self
            .get_idempotency_in_transaction(&mut tx, key_hash)
            .await?
        else {
            return Ok(false);
        };
        if record.principal_id != principal_id {
            return Err(IronCrewError::Conflict(
                "Idempotency principal changed before release".into(),
            ));
        }
        if record.attempt_id != attempt_id {
            return Err(IronCrewError::Conflict(
                "Idempotency attempt changed before release".into(),
            ));
        }
        if !record.state.is_in_flight() {
            return Ok(false);
        }
        let sql = format!(
            "DELETE FROM {} WHERE key_hash = $1 AND attempt_id = $2 \
             AND state IN ('claimed', 'running')",
            self.idempotency_table
        );
        let result = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(key_hash)
            .bind(attempt_id)
            .execute(&mut *tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!("PostgreSQL idempotency release failed: {error}"))
            })?;
        tx.commit().await.map_err(|error| {
            IronCrewError::Validation(format!(
                "PostgreSQL idempotency release commit failed: {error}"
            ))
        })?;
        Ok(result.rows_affected() == 1)
    }

    async fn prune_idempotency(&self, now: &str, limit: usize) -> Result<usize> {
        parse_timestamp("idempotency prune time", now)?;
        if limit == 0 {
            return Ok(0);
        }
        let mut tx = self.pool.begin().await.map_err(|error| {
            IronCrewError::Validation(format!(
                "PostgreSQL idempotency prune transaction failed: {error}"
            ))
        })?;
        self.lock_idempotency_quota(&mut tx).await?;
        let (database_now, _) = self
            .database_clock_with_deadline(&mut tx, 0, "idempotency pruning")
            .await?;
        let sql = format!(
            "DELETE FROM {table} WHERE key_hash IN (\
                 SELECT key_hash FROM {table} \
                 WHERE state IN ('completed', 'indeterminate') \
                   AND expires_at IS NOT NULL \
                   AND expires_at::timestamptz <= $1::timestamptz \
                 ORDER BY expires_at::timestamptz, key_hash LIMIT $2\
             )",
            table = self.idempotency_table
        );
        let result = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(&database_now)
            .bind(i64::try_from(limit).unwrap_or(i64::MAX))
            .execute(&mut *tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!("PostgreSQL idempotency prune failed: {error}"))
            })?;
        tx.commit().await.map_err(|error| {
            IronCrewError::Validation(format!(
                "PostgreSQL idempotency prune commit failed: {error}"
            ))
        })?;
        Ok(result.rows_affected() as usize)
    }

    async fn idempotency_usage(
        &self,
        principal_id: &PrincipalId,
        limits: IdempotencyLimits,
    ) -> Result<IdempotencyUsage> {
        principal_id.validate()?;
        limits.validate()?;
        let threshold = |limit: usize, percentage: usize| {
            i64::try_from(limit.saturating_mul(percentage).div_ceil(100).max(1)).unwrap_or(i64::MAX)
        };
        let sql = format!(
            "SELECT \
                 global.record_count AS global_records, \
                 global.in_flight_count AS global_in_flight, \
                 global.response_bytes AS global_response_bytes, \
                 COALESCE(principal.record_count, 0) AS principal_records, \
                 COALESCE(principal.in_flight_count, 0) AS principal_in_flight, \
                 COALESCE(principal.response_bytes, 0) AS principal_response_bytes, \
                 stats.principal_count, stats.max_principal_records, \
                 stats.max_principal_in_flight, stats.max_principal_response_bytes, \
                 stats.at_80, stats.at_90, stats.at_100 \
             FROM {accounting} AS global \
             LEFT JOIN {accounting} AS principal \
               ON principal.principal_id = $1 AND principal.is_global = FALSE \
             CROSS JOIN LATERAL (\
                 SELECT COUNT(*)::BIGINT AS principal_count, \
                        COALESCE(MAX(record_count), 0)::BIGINT AS max_principal_records, \
                        COALESCE(MAX(in_flight_count), 0)::BIGINT AS max_principal_in_flight, \
                        COALESCE(MAX(response_bytes), 0)::BIGINT AS max_principal_response_bytes, \
                        COUNT(*) FILTER (WHERE record_count >= $2 OR in_flight_count >= $3 \
                                                OR response_bytes >= $4)::BIGINT AS at_80, \
                        COUNT(*) FILTER (WHERE record_count >= $5 OR in_flight_count >= $6 \
                                                OR response_bytes >= $7)::BIGINT AS at_90, \
                        COUNT(*) FILTER (WHERE record_count >= $8 OR in_flight_count >= $9 \
                                                OR response_bytes >= $10)::BIGINT AS at_100 \
                 FROM {accounting} WHERE is_global = FALSE\
             ) AS stats \
             WHERE global.principal_id = 'global' AND global.is_global = TRUE",
            accounting = self.idempotency_accounting_table
        );
        let row = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(principal_id.as_str())
            .bind(threshold(limits.principal_max_records, 80))
            .bind(threshold(limits.principal_max_in_flight, 80))
            .bind(threshold(limits.principal_max_response_bytes, 80))
            .bind(threshold(limits.principal_max_records, 90))
            .bind(threshold(limits.principal_max_in_flight, 90))
            .bind(threshold(limits.principal_max_response_bytes, 90))
            .bind(threshold(limits.principal_max_records, 100))
            .bind(threshold(limits.principal_max_in_flight, 100))
            .bind(threshold(limits.principal_max_response_bytes, 100))
            .fetch_optional(&self.pool)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL idempotency usage query failed: {error}"
                ))
            })?
            .ok_or_else(|| {
                IronCrewError::Validation(
                    "PostgreSQL global idempotency accounting row is missing".into(),
                )
            })?;
        Ok(IdempotencyUsage {
            global_records: accounting_row_value(&row, "global_records")?,
            global_in_flight: accounting_row_value(&row, "global_in_flight")?,
            global_response_bytes: accounting_row_value(&row, "global_response_bytes")?,
            principal_records: accounting_row_value(&row, "principal_records")?,
            principal_in_flight: accounting_row_value(&row, "principal_in_flight")?,
            principal_response_bytes: accounting_row_value(&row, "principal_response_bytes")?,
            principal_count: accounting_row_value(&row, "principal_count")?,
            max_principal_records: accounting_row_value(&row, "max_principal_records")?,
            max_principal_in_flight: accounting_row_value(&row, "max_principal_in_flight")?,
            max_principal_response_bytes: accounting_row_value(
                &row,
                "max_principal_response_bytes",
            )?,
            principals_at_or_above_80_percent: accounting_row_value(&row, "at_80")?,
            principals_at_or_above_90_percent: accounting_row_value(&row, "at_90")?,
            principals_at_or_above_100_percent: accounting_row_value(&row, "at_100")?,
        })
    }

    // ─── Persistent sessions ────────────────────────────────────────────────

    async fn save_conversation(&self, record: &ConversationRecord) -> Result<u64> {
        let messages_json = serde_json::to_string(&record.messages).map_err(|e| {
            IronCrewError::Validation(format!("Failed to serialize messages: {}", e))
        })?;
        let expected_revision = i64::try_from(record.revision).map_err(|_| {
            IronCrewError::Validation("Conversation revision is out of range".into())
        })?;
        let mut tx = self.pool.begin().await.map_err(|e| {
            IronCrewError::Validation(format!(
                "PostgreSQL save_conversation transaction error: {e}"
            ))
        })?;
        self.lock_resource(
            &mut tx,
            CONVERSATION_MESSAGE_OPERATION,
            record.flow_path.as_deref().unwrap_or(""),
            &record.id,
        )
        .await?;
        let guard_sql = format!(
            "SELECT EXISTS (SELECT 1 FROM {} \
             WHERE operation = $1 AND scope = $2 AND resource_id = $3 \
               AND state IN ('claimed', 'running'))",
            self.idempotency_table
        );
        let active: bool = sqlx::query_scalar(sqlx::AssertSqlSafe(guard_sql))
            .bind(CONVERSATION_MESSAGE_OPERATION)
            .bind(record.flow_path.as_deref().unwrap_or(""))
            .bind(&record.id)
            .fetch_one(&mut *tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL conversation idempotency guard failed: {error}"
                ))
            })?;
        if active {
            return Err(IronCrewError::Conflict(format!(
                "Conversation '{}' has an active idempotent message operation",
                record.id
            )));
        }
        let select_sql = format!(
            "SELECT revision FROM {} \
             WHERE id = $1 AND flow_path IS NOT DISTINCT FROM $2 FOR UPDATE",
            self.conversations_table
        );
        let current: Option<i64> = sqlx::query_scalar(sqlx::AssertSqlSafe(select_sql))
            .bind(&record.id)
            .bind(&record.flow_path)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|e| {
                IronCrewError::Validation(format!(
                    "PostgreSQL save_conversation revision read error: {e}"
                ))
            })?;
        let revision: Option<i64> = match current {
            None if expected_revision == 0 => {
                let insert_sql = format!(
                    "INSERT INTO {} \
                     (id, flow_name, flow_path, agent_name, messages, created_at, updated_at, revision) \
                     VALUES ($1, $2, $3, $4, $5::jsonb, $6, $7, 1) \
                     ON CONFLICT (flow_path, id) DO NOTHING RETURNING revision",
                    self.conversations_table
                );
                sqlx::query_scalar(sqlx::AssertSqlSafe(insert_sql))
                    .bind(&record.id)
                    .bind(&record.flow_name)
                    .bind(&record.flow_path)
                    .bind(&record.agent_name)
                    .bind(&messages_json)
                    .bind(&record.created_at)
                    .bind(&record.updated_at)
                    .fetch_optional(&mut *tx)
                    .await
                    .map_err(|e| {
                        IronCrewError::Validation(format!(
                            "PostgreSQL save_conversation insert error: {e}"
                        ))
                    })?
            }
            Some(current) if current == expected_revision => {
                let update_sql = format!(
                    "UPDATE {} SET flow_name = $3, agent_name = $4, \
                     messages = $5::jsonb, created_at = $6, updated_at = $7, \
                     revision = revision + 1 \
                     WHERE id = $1 AND flow_path IS NOT DISTINCT FROM $2 AND revision = $8 \
                     RETURNING revision",
                    self.conversations_table
                );
                sqlx::query_scalar(sqlx::AssertSqlSafe(update_sql))
                    .bind(&record.id)
                    .bind(&record.flow_path)
                    .bind(&record.flow_name)
                    .bind(&record.agent_name)
                    .bind(&messages_json)
                    .bind(&record.created_at)
                    .bind(&record.updated_at)
                    .bind(expected_revision)
                    .fetch_optional(&mut *tx)
                    .await
                    .map_err(|e| {
                        IronCrewError::Validation(format!(
                            "PostgreSQL save_conversation update error: {e}"
                        ))
                    })?
            }
            _ => None,
        };
        let revision = revision.ok_or_else(|| {
            IronCrewError::Conflict(format!(
                "Conversation '{}' changed since revision {}; reopen it before saving",
                record.id, record.revision
            ))
        })?;
        tx.commit().await.map_err(|e| {
            IronCrewError::Validation(format!("PostgreSQL save_conversation commit error: {e}"))
        })?;
        u64::try_from(revision)
            .map_err(|_| IronCrewError::Validation("Invalid conversation revision".into()))
    }

    async fn get_conversation(
        &self,
        flow_path: Option<&str>,
        id: &str,
    ) -> Result<Option<ConversationRecord>> {
        // Flow-scoped lookup: when `flow_path` is Some, require an exact
        // match. `$2::TEXT IS NULL` lets the same query serve global
        // (unscoped) admin lookups.
        let sql = format!(
            "SELECT id, flow_name, flow_path, agent_name, messages::text, created_at, updated_at, revision \
             FROM {} WHERE id = $1 AND ($2::TEXT IS NULL OR flow_path = $2)",
            self.conversations_table
        );
        let row_opt = sqlx::query(sqlx::AssertSqlSafe(sql.to_string()))
            .bind(id)
            .bind(flow_path)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| {
                IronCrewError::Validation(format!("PostgreSQL get_conversation error: {}", e))
            })?;
        let Some(row) = row_opt else {
            return Ok(None);
        };
        let messages_str: String = row
            .try_get("messages")
            .map_err(|e| IronCrewError::Validation(format!("Column error: {}", e)))?;
        Ok(Some(ConversationRecord {
            id: row
                .try_get("id")
                .map_err(|e| IronCrewError::Validation(e.to_string()))?,
            flow_name: row
                .try_get("flow_name")
                .map_err(|e| IronCrewError::Validation(e.to_string()))?,
            flow_path: row
                .try_get("flow_path")
                .map_err(|e| IronCrewError::Validation(e.to_string()))?,
            agent_name: row
                .try_get("agent_name")
                .map_err(|e| IronCrewError::Validation(e.to_string()))?,
            messages: decode_stored_json(&messages_str, "conversations.messages")?,
            created_at: row
                .try_get("created_at")
                .map_err(|e| IronCrewError::Validation(e.to_string()))?,
            updated_at: row
                .try_get("updated_at")
                .map_err(|e| IronCrewError::Validation(e.to_string()))?,
            revision: u64::try_from(
                row.try_get::<i64, _>("revision")
                    .map_err(|e| IronCrewError::Validation(e.to_string()))?,
            )
            .map_err(|_| IronCrewError::Validation("Invalid conversation revision".into()))?,
        }))
    }

    async fn delete_conversation(&self, flow_path: Option<&str>, id: &str) -> Result<()> {
        let mut tx = self.pool.begin().await.map_err(|error| {
            IronCrewError::Validation(format!(
                "PostgreSQL delete_conversation transaction error: {error}"
            ))
        })?;
        self.lock_resource(
            &mut tx,
            CONVERSATION_MESSAGE_OPERATION,
            flow_path.unwrap_or(""),
            id,
        )
        .await?;
        let guard_sql = format!(
            "SELECT EXISTS (SELECT 1 FROM {} \
             WHERE operation = $1 AND scope = $2 AND resource_id = $3 \
               AND state IN ('claimed', 'running'))",
            self.idempotency_table
        );
        let active: bool = sqlx::query_scalar(sqlx::AssertSqlSafe(guard_sql))
            .bind(CONVERSATION_MESSAGE_OPERATION)
            .bind(flow_path.unwrap_or(""))
            .bind(id)
            .fetch_one(&mut *tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL conversation delete idempotency guard failed: {error}"
                ))
            })?;
        if active {
            return Err(IronCrewError::Conflict(format!(
                "Conversation '{id}' has an active idempotent message operation"
            )));
        }
        let sql = format!(
            "DELETE FROM {} WHERE id = $1 AND ($2::TEXT IS NULL OR flow_path = $2)",
            self.conversations_table
        );
        sqlx::query(sqlx::AssertSqlSafe(sql.to_string()))
            .bind(id)
            .bind(flow_path)
            .execute(&mut *tx)
            .await
            .map_err(|e| {
                IronCrewError::Validation(format!("PostgreSQL delete_conversation error: {}", e))
            })?;
        tx.commit().await.map_err(|error| {
            IronCrewError::Validation(format!(
                "PostgreSQL delete_conversation commit error: {error}"
            ))
        })?;
        Ok(())
    }

    async fn list_conversations(
        &self,
        flow_path: Option<&str>,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<ConversationSummary>> {
        let sql = format!(
            "SELECT c.id, c.flow_path, c.agent_name, \
                    (SELECT COUNT(*) FROM jsonb_array_elements(c.messages) AS message \
                     WHERE message->>'role' = 'user') AS turn_count, \
                    c.created_at, c.updated_at \
             FROM {} AS c \
             WHERE ($1::TEXT IS NULL OR c.flow_path = $1) \
             ORDER BY c.updated_at DESC \
             LIMIT $2 OFFSET $3",
            self.conversations_table
        );
        let limit_i = if limit == 0 { i64::MAX } else { limit as i64 };
        let rows = sqlx::query(sqlx::AssertSqlSafe(sql.to_string()))
            .bind(flow_path)
            .bind(limit_i)
            .bind(offset as i64)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| {
                IronCrewError::Validation(format!("PostgreSQL list_conversations error: {}", e))
            })?;
        let mut summaries = Vec::with_capacity(rows.len());
        for row in rows.iter() {
            let turn_count: i64 = row
                .try_get("turn_count")
                .map_err(|e| IronCrewError::Validation(format!("Column error: {}", e)))?;
            summaries.push(ConversationSummary {
                id: row
                    .try_get("id")
                    .map_err(|e| IronCrewError::Validation(e.to_string()))?,
                flow_path: row
                    .try_get("flow_path")
                    .map_err(|e| IronCrewError::Validation(e.to_string()))?,
                agent_name: row
                    .try_get("agent_name")
                    .map_err(|e| IronCrewError::Validation(e.to_string()))?,
                created_at: row
                    .try_get("created_at")
                    .map_err(|e| IronCrewError::Validation(e.to_string()))?,
                updated_at: row
                    .try_get("updated_at")
                    .map_err(|e| IronCrewError::Validation(e.to_string()))?,
                turn_count: usize::try_from(turn_count).map_err(|_| {
                    IronCrewError::Validation("PostgreSQL turn_count is out of range".into())
                })?,
            });
        }
        Ok(summaries)
    }

    async fn count_conversations(&self, flow_path: Option<&str>) -> Result<u64> {
        let sql = format!(
            "SELECT COUNT(*) FROM {} \
             WHERE ($1::TEXT IS NULL OR flow_path = $1)",
            self.conversations_table
        );
        let row = sqlx::query(sqlx::AssertSqlSafe(sql.to_string()))
            .bind(flow_path)
            .fetch_one(&self.pool)
            .await
            .map_err(|e| {
                IronCrewError::Validation(format!("PostgreSQL count_conversations error: {}", e))
            })?;
        let count: i64 = row
            .try_get(0)
            .map_err(|e| IronCrewError::Validation(format!("Column error: {}", e)))?;
        Ok(count as u64)
    }

    async fn save_dialog_state(&self, record: &DialogStateRecord) -> Result<u64> {
        let agents_json = serde_json::to_string(&record.agent_names).map_err(|e| {
            IronCrewError::Validation(format!("Failed to serialize agent_names: {}", e))
        })?;
        let transcript_json = serde_json::to_string(&record.transcript).map_err(|e| {
            IronCrewError::Validation(format!("Failed to serialize transcript: {}", e))
        })?;
        let expected_revision = i64::try_from(record.revision)
            .map_err(|_| IronCrewError::Validation("Dialog revision is out of range".into()))?;
        let mut tx = self.pool.begin().await.map_err(|e| {
            IronCrewError::Validation(format!(
                "PostgreSQL save_dialog_state transaction error: {e}"
            ))
        })?;
        let select_sql = format!(
            "SELECT revision FROM {} \
             WHERE id = $1 AND flow_path IS NOT DISTINCT FROM $2 FOR UPDATE",
            self.dialogs_table
        );
        let current: Option<i64> = sqlx::query_scalar(sqlx::AssertSqlSafe(select_sql))
            .bind(&record.id)
            .bind(&record.flow_path)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|e| {
                IronCrewError::Validation(format!(
                    "PostgreSQL save_dialog_state revision read error: {e}"
                ))
            })?;
        let revision: Option<i64> = match current {
            None if expected_revision == 0 => {
                let insert_sql = format!(
                    "INSERT INTO {} \
                     (id, flow_name, flow_path, agent_names, starter, transcript, next_index, stopped, stop_reason, created_at, updated_at, revision) \
                     VALUES ($1, $2, $3, $4::jsonb, $5, $6::jsonb, $7, $8, $9, $10, $11, 1) \
                     ON CONFLICT (flow_path, id) DO NOTHING RETURNING revision",
                    self.dialogs_table
                );
                sqlx::query_scalar(sqlx::AssertSqlSafe(insert_sql))
                    .bind(&record.id)
                    .bind(&record.flow_name)
                    .bind(&record.flow_path)
                    .bind(&agents_json)
                    .bind(&record.starter)
                    .bind(&transcript_json)
                    .bind(record.next_index as i32)
                    .bind(record.stopped)
                    .bind(&record.stop_reason)
                    .bind(&record.created_at)
                    .bind(&record.updated_at)
                    .fetch_optional(&mut *tx)
                    .await
                    .map_err(|e| {
                        IronCrewError::Validation(format!(
                            "PostgreSQL save_dialog_state insert error: {e}"
                        ))
                    })?
            }
            Some(current) if current == expected_revision => {
                let update_sql = format!(
                    "UPDATE {} SET flow_name = $3, agent_names = $4::jsonb, \
                     starter = $5, transcript = $6::jsonb, next_index = $7, \
                     stopped = $8, stop_reason = $9, created_at = $10, \
                     updated_at = $11, revision = revision + 1 \
                     WHERE id = $1 AND flow_path IS NOT DISTINCT FROM $2 AND revision = $12 \
                     RETURNING revision",
                    self.dialogs_table
                );
                sqlx::query_scalar(sqlx::AssertSqlSafe(update_sql))
                    .bind(&record.id)
                    .bind(&record.flow_path)
                    .bind(&record.flow_name)
                    .bind(&agents_json)
                    .bind(&record.starter)
                    .bind(&transcript_json)
                    .bind(record.next_index as i32)
                    .bind(record.stopped)
                    .bind(&record.stop_reason)
                    .bind(&record.created_at)
                    .bind(&record.updated_at)
                    .bind(expected_revision)
                    .fetch_optional(&mut *tx)
                    .await
                    .map_err(|e| {
                        IronCrewError::Validation(format!(
                            "PostgreSQL save_dialog_state update error: {e}"
                        ))
                    })?
            }
            _ => None,
        };
        let revision = revision.ok_or_else(|| {
            IronCrewError::Conflict(format!(
                "Dialog '{}' changed since revision {}; reopen it before saving",
                record.id, record.revision
            ))
        })?;
        tx.commit().await.map_err(|e| {
            IronCrewError::Validation(format!("PostgreSQL save_dialog_state commit error: {e}"))
        })?;
        u64::try_from(revision)
            .map_err(|_| IronCrewError::Validation("Invalid dialog revision".into()))
    }

    async fn get_dialog_state(
        &self,
        flow_path: Option<&str>,
        id: &str,
    ) -> Result<Option<DialogStateRecord>> {
        let sql = format!(
            "SELECT id, flow_name, flow_path, agent_names::text, starter, transcript::text, \
             next_index, stopped, stop_reason, created_at, updated_at, revision \
             FROM {} WHERE id = $1 AND ($2::TEXT IS NULL OR flow_path = $2)",
            self.dialogs_table
        );
        let row_opt = sqlx::query(sqlx::AssertSqlSafe(sql.to_string()))
            .bind(id)
            .bind(flow_path)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| {
                IronCrewError::Validation(format!("PostgreSQL get_dialog_state error: {}", e))
            })?;
        let Some(row) = row_opt else {
            return Ok(None);
        };
        let agents_str: String = row
            .try_get("agent_names")
            .map_err(|e| IronCrewError::Validation(e.to_string()))?;
        let transcript_str: String = row
            .try_get("transcript")
            .map_err(|e| IronCrewError::Validation(e.to_string()))?;
        let next_index_i32: i32 = row
            .try_get("next_index")
            .map_err(|e| IronCrewError::Validation(e.to_string()))?;
        Ok(Some(DialogStateRecord {
            id: row
                .try_get("id")
                .map_err(|e| IronCrewError::Validation(e.to_string()))?,
            flow_name: row
                .try_get("flow_name")
                .map_err(|e| IronCrewError::Validation(e.to_string()))?,
            flow_path: row
                .try_get("flow_path")
                .map_err(|e| IronCrewError::Validation(e.to_string()))?,
            agent_names: decode_stored_json(&agents_str, "dialogs.agent_names")?,
            starter: row
                .try_get("starter")
                .map_err(|e| IronCrewError::Validation(e.to_string()))?,
            transcript: decode_stored_json(&transcript_str, "dialogs.transcript")?,
            next_index: next_index_i32.max(0) as usize,
            stopped: row
                .try_get("stopped")
                .map_err(|e| IronCrewError::Validation(e.to_string()))?,
            stop_reason: row
                .try_get("stop_reason")
                .map_err(|e| IronCrewError::Validation(e.to_string()))?,
            created_at: row
                .try_get("created_at")
                .map_err(|e| IronCrewError::Validation(e.to_string()))?,
            updated_at: row
                .try_get("updated_at")
                .map_err(|e| IronCrewError::Validation(e.to_string()))?,
            revision: u64::try_from(
                row.try_get::<i64, _>("revision")
                    .map_err(|e| IronCrewError::Validation(e.to_string()))?,
            )
            .map_err(|_| IronCrewError::Validation("Invalid dialog revision".into()))?,
        }))
    }

    async fn delete_dialog_state(&self, flow_path: Option<&str>, id: &str) -> Result<()> {
        let sql = format!(
            "DELETE FROM {} WHERE id = $1 AND ($2::TEXT IS NULL OR flow_path = $2)",
            self.dialogs_table
        );
        sqlx::query(sqlx::AssertSqlSafe(sql.to_string()))
            .bind(id)
            .bind(flow_path)
            .execute(&self.pool)
            .await
            .map_err(|e| {
                IronCrewError::Validation(format!("PostgreSQL delete_dialog_state error: {}", e))
            })?;
        Ok(())
    }

    async fn save_audit_event(&self, event: &crate::engine::audit::AuditEvent) -> Result<String> {
        let id = uuid::Uuid::new_v4().to_string();
        let sql = format!(
            "INSERT INTO {at}
             (id, timestamp, action, flow_path, target, actor, source_ip, success, status_code, metadata)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10::jsonb)",
            at = self.audit_events_table
        );
        let metadata_str = match &event.metadata {
            Some(v) => Some(
                serde_json::to_string(v)
                    .map_err(|e| IronCrewError::Validation(format!("Metadata serialize: {}", e)))?,
            ),
            None => None,
        };
        sqlx::query(sqlx::AssertSqlSafe(sql.to_string()))
            .bind(&id)
            .bind(&event.timestamp)
            .bind(&event.action)
            .bind(&event.flow_path)
            .bind(&event.target)
            .bind(&event.actor)
            .bind(&event.source_ip)
            .bind(event.success)
            .bind(event.status_code as i32)
            .bind(metadata_str)
            .execute(&self.pool)
            .await
            .map_err(|e| IronCrewError::Validation(format!("PG insert audit: {}", e)))?;
        tracing::debug!("Audit event saved: {}", id);
        Ok(id)
    }

    async fn list_audit_events(
        &self,
        filter: &crate::engine::audit::AuditFilter,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<crate::engine::audit::AuditEvent>> {
        let WhereClause {
            sql: where_sql,
            params,
        } = store_sql::audit_where(filter, Dialect::Postgres);
        let mut sql = format!(
            "SELECT id, timestamp, action, flow_path, target, actor, source_ip, success, status_code, metadata::text
             FROM {}{}",
            self.audit_events_table, where_sql
        );
        sql.push_str(" ORDER BY timestamp DESC");
        if limit > 0 {
            sql.push_str(&format!(" LIMIT {}", limit));
        }
        if offset > 0 {
            sql.push_str(&format!(" OFFSET {}", offset));
        }

        let q = sqlx::query(sqlx::AssertSqlSafe(sql.to_string()));
        let q = bind_params(q, &params);

        let rows = q
            .fetch_all(&self.pool)
            .await
            .map_err(|e| IronCrewError::Validation(format!("PG list audit: {}", e)))?;

        let mut events = Vec::new();
        for row in rows {
            let metadata_str: Option<String> = row
                .try_get("metadata")
                .map_err(|e| IronCrewError::Validation(format!("Column error: {e}")))?;
            let metadata = metadata_str
                .as_deref()
                .map(|raw| decode_stored_json(raw, "audit_events.metadata"))
                .transpose()?;
            events.push(crate::engine::audit::AuditEvent {
                id: row
                    .try_get("id")
                    .map_err(|e| IronCrewError::Validation(e.to_string()))?,
                timestamp: row
                    .try_get("timestamp")
                    .map_err(|e| IronCrewError::Validation(e.to_string()))?,
                action: row
                    .try_get("action")
                    .map_err(|e| IronCrewError::Validation(e.to_string()))?,
                flow_path: row.try_get("flow_path").ok(),
                target: row.try_get("target").ok(),
                actor: row.try_get("actor").ok(),
                source_ip: row.try_get("source_ip").ok(),
                success: row
                    .try_get("success")
                    .map_err(|e| IronCrewError::Validation(e.to_string()))?,
                status_code: row
                    .try_get::<i32, _>("status_code")
                    .map_err(|e| IronCrewError::Validation(e.to_string()))?
                    as u16,
                metadata,
            });
        }
        Ok(events)
    }

    async fn count_audit_events(&self, filter: &crate::engine::audit::AuditFilter) -> Result<u64> {
        let WhereClause {
            sql: where_sql,
            params,
        } = store_sql::audit_where(filter, Dialect::Postgres);
        let sql = format!(
            "SELECT COUNT(*) FROM {}{}",
            self.audit_events_table, where_sql
        );

        let mut q = sqlx::query_scalar::<_, i64>(sqlx::AssertSqlSafe(sql.to_string()));
        for p in &params {
            q = match p {
                SqlParam::Text(s) => q.bind(s),
                SqlParam::Bool(b) => q.bind(b),
            };
        }

        let count = q
            .fetch_one(&self.pool)
            .await
            .map_err(|e| IronCrewError::Validation(format!("PG count audit: {}", e)))?;
        Ok(count as u64)
    }
}

fn row_to_record(row: &sqlx::postgres::PgRow) -> Result<RunRecord> {
    let status_str: String = row
        .try_get("status")
        .map_err(|e| IronCrewError::Validation(format!("Column error: {}", e)))?;
    let task_results_str: String = row
        .try_get("task_results")
        .map_err(|e| IronCrewError::Validation(format!("Column error: {}", e)))?;
    let tags_str: String = row
        .try_get("tags")
        .map_err(|e| IronCrewError::Validation(format!("Column error: {}", e)))?;
    let duration_ms: i64 = row
        .try_get("duration_ms")
        .map_err(|e| IronCrewError::Validation(format!("Column error: {}", e)))?;
    let agent_count: i32 = row
        .try_get("agent_count")
        .map_err(|e| IronCrewError::Validation(format!("Column error: {}", e)))?;
    let task_count: i32 = row
        .try_get("task_count")
        .map_err(|e| IronCrewError::Validation(format!("Column error: {}", e)))?;
    let total_tokens: i32 = row
        .try_get("total_tokens")
        .map_err(|e| IronCrewError::Validation(format!("Column error: {}", e)))?;
    let cached_tokens: i32 = row
        .try_get("cached_tokens")
        .map_err(|e| IronCrewError::Validation(format!("Column error: {}", e)))?;

    Ok(RunRecord {
        run_id: row
            .try_get("run_id")
            .map_err(|e| IronCrewError::Validation(format!("Column error: {}", e)))?,
        flow_name: row
            .try_get("flow_name")
            .map_err(|e| IronCrewError::Validation(format!("Column error: {}", e)))?,
        flow: row
            .try_get("flow")
            .map_err(|e| IronCrewError::Validation(format!("Column error: {}", e)))?,
        status: status_str.parse::<RunStatus>()?,
        started_at: row
            .try_get("started_at")
            .map_err(|e| IronCrewError::Validation(format!("Column error: {}", e)))?,
        finished_at: row
            .try_get("finished_at")
            .map_err(|e| IronCrewError::Validation(format!("Column error: {}", e)))?,
        duration_ms: duration_ms as u64,
        task_results: decode_stored_json(&task_results_str, "runs.task_results")?,
        agent_count: agent_count as usize,
        task_count: task_count as usize,
        total_tokens: total_tokens as u32,
        cached_tokens: cached_tokens as u32,
        tags: decode_stored_json(&tags_str, "runs.tags")?,
        owner_instance_id: row
            .try_get("owner_instance_id")
            .map_err(|e| IronCrewError::Validation(format!("Column error: {}", e)))?,
        lease_expires_at: row
            .try_get("lease_expires_at")
            .map_err(|e| IronCrewError::Validation(format!("Column error: {}", e)))?,
    })
}

/// Convert a row from the summary query into a RunSummary (no task_results).
fn row_to_summary(row: &sqlx::postgres::PgRow) -> Result<RunSummary> {
    let status_str: String = row
        .try_get("status")
        .map_err(|e| IronCrewError::Validation(format!("Column error: {}", e)))?;
    let tags_str: String = row
        .try_get("tags")
        .map_err(|e| IronCrewError::Validation(format!("Column error: {}", e)))?;
    let duration_ms: i64 = row
        .try_get("duration_ms")
        .map_err(|e| IronCrewError::Validation(format!("Column error: {}", e)))?;
    let agent_count: i32 = row
        .try_get("agent_count")
        .map_err(|e| IronCrewError::Validation(format!("Column error: {}", e)))?;
    let task_count: i32 = row
        .try_get("task_count")
        .map_err(|e| IronCrewError::Validation(format!("Column error: {}", e)))?;
    let total_tokens: i32 = row
        .try_get("total_tokens")
        .map_err(|e| IronCrewError::Validation(format!("Column error: {}", e)))?;
    let cached_tokens: i32 = row
        .try_get("cached_tokens")
        .map_err(|e| IronCrewError::Validation(format!("Column error: {}", e)))?;

    Ok(RunSummary {
        run_id: row
            .try_get("run_id")
            .map_err(|e| IronCrewError::Validation(format!("Column error: {}", e)))?,
        flow_name: row
            .try_get("flow_name")
            .map_err(|e| IronCrewError::Validation(format!("Column error: {}", e)))?,
        flow: row
            .try_get("flow")
            .map_err(|e| IronCrewError::Validation(format!("Column error: {}", e)))?,
        status: status_str.parse::<RunStatus>()?,
        started_at: row
            .try_get("started_at")
            .map_err(|e| IronCrewError::Validation(format!("Column error: {}", e)))?,
        finished_at: row
            .try_get("finished_at")
            .map_err(|e| IronCrewError::Validation(format!("Column error: {}", e)))?,
        duration_ms: duration_ms as u64,
        agent_count: agent_count as usize,
        task_count: task_count as usize,
        total_tokens: total_tokens as u32,
        cached_tokens: cached_tokens as u32,
        tags: decode_stored_json(&tags_str, "runs.tags")?,
    })
}

fn nonnegative_accounting_value(label: &str, value: i64) -> Result<usize> {
    usize::try_from(value).map_err(|_| {
        IronCrewError::Validation(format!(
            "PostgreSQL idempotency accounting value '{label}' is negative or out of range"
        ))
    })
}

fn decode_idempotency_accounting(values: (i64, i64, i64)) -> Result<IdempotencyAccounting> {
    Ok(IdempotencyAccounting {
        records: nonnegative_accounting_value("record_count", values.0)?,
        in_flight: nonnegative_accounting_value("in_flight_count", values.1)?,
        response_bytes: nonnegative_accounting_value("response_bytes", values.2)?,
    })
}

fn accounting_row_value(row: &sqlx::postgres::PgRow, column: &str) -> Result<usize> {
    let value = row
        .try_get::<i64, _>(column)
        .map_err(|error| IronCrewError::Validation(format!("Column error: {error}")))?;
    nonnegative_accounting_value(column, value)
}

fn row_to_idempotency_record(row: &sqlx::postgres::PgRow) -> Result<IdempotencyRecord> {
    let base_revision = row
        .try_get::<Option<i64>, _>("base_revision")
        .map_err(|e| IronCrewError::Validation(format!("Column error: {e}")))?
        .map(u64::try_from)
        .transpose()
        .map_err(|_| {
            IronCrewError::Validation("PostgreSQL idempotency base_revision is negative".into())
        })?;
    let response_status = row
        .try_get::<Option<i32>, _>("response_status")
        .map_err(|e| IronCrewError::Validation(format!("Column error: {e}")))?
        .map(u16::try_from)
        .transpose()
        .map_err(|_| {
            IronCrewError::Validation(
                "PostgreSQL idempotency response_status is out of range".into(),
            )
        })?;
    let ttl_seconds = u64::try_from(
        row.try_get::<i64, _>("ttl_seconds")
            .map_err(|e| IronCrewError::Validation(format!("Column error: {e}")))?,
    )
    .map_err(|_| {
        IronCrewError::Validation("PostgreSQL idempotency ttl_seconds is negative".into())
    })?;
    let state = row
        .try_get::<String, _>("state")
        .map_err(|e| IronCrewError::Validation(format!("Column error: {e}")))?
        .parse::<IdempotencyState>()?;
    let lease_expires_at: String = row
        .try_get("lease_expires_at")
        .map_err(|e| IronCrewError::Validation(format!("Column error: {e}")))?;
    let lease_expires_at = if lease_expires_at.is_empty() {
        lease_expires_at
    } else {
        canonical_timestamp("stored idempotency lease expiry", &lease_expires_at)?
    };
    let created_at = canonical_timestamp(
        "stored idempotency creation time",
        &row.try_get::<String, _>("created_at")
            .map_err(|e| IronCrewError::Validation(format!("Column error: {e}")))?,
    )?;
    let updated_at = canonical_timestamp(
        "stored idempotency update time",
        &row.try_get::<String, _>("updated_at")
            .map_err(|e| IronCrewError::Validation(format!("Column error: {e}")))?,
    )?;
    let completed_at = row
        .try_get::<Option<String>, _>("completed_at")
        .map_err(|e| IronCrewError::Validation(format!("Column error: {e}")))?
        .map(|value| canonical_timestamp("stored idempotency completion time", &value))
        .transpose()?;
    let expires_at = row
        .try_get::<Option<String>, _>("expires_at")
        .map_err(|e| IronCrewError::Validation(format!("Column error: {e}")))?
        .map(|value| canonical_timestamp("stored idempotency retention expiry", &value))
        .transpose()?;

    let record = IdempotencyRecord {
        key_hash: row
            .try_get("key_hash")
            .map_err(|e| IronCrewError::Validation(format!("Column error: {e}")))?,
        principal_id: PrincipalId::from_digest(
            row.try_get("principal_id")
                .map_err(|e| IronCrewError::Validation(format!("Column error: {e}")))?,
        )?,
        request_fingerprint: row
            .try_get("request_fingerprint")
            .map_err(|e| IronCrewError::Validation(format!("Column error: {e}")))?,
        operation: row
            .try_get("operation")
            .map_err(|e| IronCrewError::Validation(format!("Column error: {e}")))?,
        scope: row
            .try_get("scope")
            .map_err(|e| IronCrewError::Validation(format!("Column error: {e}")))?,
        resource_id: row
            .try_get("resource_id")
            .map_err(|e| IronCrewError::Validation(format!("Column error: {e}")))?,
        exclusive_scope: row
            .try_get("exclusive_scope")
            .map_err(|e| IronCrewError::Validation(format!("Column error: {e}")))?,
        attempt_id: row
            .try_get("attempt_id")
            .map_err(|e| IronCrewError::Validation(format!("Column error: {e}")))?,
        owner_instance_id: row
            .try_get("owner_instance_id")
            .map_err(|e| IronCrewError::Validation(format!("Column error: {e}")))?,
        base_revision,
        state,
        response_status,
        response_body: row
            .try_get("response_body")
            .map_err(|e| IronCrewError::Validation(format!("Column error: {e}")))?,
        lease_expires_at,
        created_at,
        updated_at,
        completed_at,
        expires_at,
        ttl_seconds,
    };
    record.validate()?;
    Ok(record)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_grows_exponentially_then_caps() {
        let base = 1_000;
        let cap = CONNECT_BACKOFF_CAP_MS;
        assert_eq!(retry_backoff(1, base, cap), Duration::from_millis(1_000));
        assert_eq!(retry_backoff(2, base, cap), Duration::from_millis(2_000));
        assert_eq!(retry_backoff(3, base, cap), Duration::from_millis(4_000));
        assert_eq!(retry_backoff(4, base, cap), Duration::from_millis(8_000));
        assert_eq!(retry_backoff(5, base, cap), Duration::from_millis(16_000));
        // 1_000 * 2^5 = 32_000 -> capped at 30_000
        assert_eq!(retry_backoff(6, base, cap), Duration::from_millis(cap));
        assert_eq!(retry_backoff(10, base, cap), Duration::from_millis(cap));
    }

    #[test]
    fn backoff_does_not_overflow_on_large_attempts() {
        // Shift would exceed u64 width; must saturate to the cap, not panic.
        assert_eq!(
            retry_backoff(1_000, 1_000, CONNECT_BACKOFF_CAP_MS),
            Duration::from_millis(CONNECT_BACKOFF_CAP_MS)
        );
    }

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
