use async_trait::async_trait;
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use std::sync::OnceLock;
use std::time::Duration;

use crate::engine::audit::{AuditEvent, AuditFilter};
use crate::engine::idempotency::{
    ConversationIdempotencyCommit, IdempotencyClaim, IdempotencyClaimOutcome,
    IdempotencyCompletion, IdempotencyCompletionOutcome, IdempotencyLimits, IdempotencyLookup,
    IdempotencyUsage, PrincipalId, RunFenceHeartbeat,
};
use crate::engine::run_history::{
    ListRunsFilter, RunCompletion, RunIntent, RunRecord, RunStatus, RunSummary, RunTransition,
};
use crate::engine::sessions::{ConversationRecord, ConversationSummary, DialogStateRecord};
use crate::utils::error::Result;

pub const DEFAULT_RUN_LEASE_TTL_SECONDS: u64 = 60;
pub const MAX_RUN_LEASE_TTL_SECONDS: u64 = 86_400;

static PROCESS_INSTANCE_ID: OnceLock<String> = OnceLock::new();

/// Ownership configuration attached to every store handle in this process.
///
/// The generated id is stable for the lifetime of the process. Deployments can
/// provide `IRONCREW_INSTANCE_ID` (for example, an OpenShift pod UID) when they
/// already have a suitably unique runtime identity. The lease TTL controls how
/// long another process waits after the last heartbeat before it may reconcile
/// this process's unfinished runs.
#[derive(Debug, Clone)]
pub struct RunLeaseConfig {
    instance_id: String,
    ttl: Duration,
}

impl RunLeaseConfig {
    pub fn from_env() -> Result<Self> {
        let configured_instance_id = match std::env::var("IRONCREW_INSTANCE_ID") {
            Ok(value) => Some(validate_instance_id(value)?),
            Err(std::env::VarError::NotPresent) => None,
            Err(std::env::VarError::NotUnicode(_)) => {
                return Err(crate::utils::error::IronCrewError::Validation(
                    "IRONCREW_INSTANCE_ID must be valid UTF-8".into(),
                ));
            }
        };
        // Cache configured ids too: all independently constructed stores in
        // this process must agree even if environment loading/mutation happens
        // later in startup.
        let instance_id = PROCESS_INSTANCE_ID
            .get_or_init(|| {
                configured_instance_id.unwrap_or_else(|| {
                    format!("ironcrew-{}-{}", std::process::id(), uuid::Uuid::new_v4())
                })
            })
            .clone();

        let ttl_seconds = match std::env::var("IRONCREW_RUN_LEASE_TTL_SECONDS") {
            Ok(value) => value.parse::<u64>().map_err(|_| {
                crate::utils::error::IronCrewError::Validation(
                    "IRONCREW_RUN_LEASE_TTL_SECONDS must be a positive integer".into(),
                )
            })?,
            Err(std::env::VarError::NotPresent) => DEFAULT_RUN_LEASE_TTL_SECONDS,
            Err(std::env::VarError::NotUnicode(_)) => {
                return Err(crate::utils::error::IronCrewError::Validation(
                    "IRONCREW_RUN_LEASE_TTL_SECONDS must be valid UTF-8".into(),
                ));
            }
        };
        if ttl_seconds == 0 || ttl_seconds > MAX_RUN_LEASE_TTL_SECONDS {
            return Err(crate::utils::error::IronCrewError::Validation(
                "IRONCREW_RUN_LEASE_TTL_SECONDS must be between 1 and 86400".into(),
            ));
        }

        Self::new(instance_id, Duration::from_secs(ttl_seconds))
    }

    /// Explicit constructor used by deterministic storage and multi-instance
    /// tests. Production callers should normally use [`Self::from_env`].
    pub fn new(instance_id: impl Into<String>, ttl: Duration) -> Result<Self> {
        let instance_id = validate_instance_id(instance_id.into())?;
        if ttl < Duration::from_secs(1) || ttl.as_secs() > MAX_RUN_LEASE_TTL_SECONDS {
            return Err(crate::utils::error::IronCrewError::Validation(
                "Run lease TTL must be between 1 second and 24 hours".into(),
            ));
        }
        Ok(Self { instance_id, ttl })
    }

    pub fn instance_id(&self) -> &str {
        &self.instance_id
    }

    pub fn ttl(&self) -> Duration {
        self.ttl
    }

    pub fn deadline_from(&self, now: DateTime<Utc>) -> String {
        let ttl = ChronoDuration::seconds(self.ttl.as_secs() as i64);
        now.checked_add_signed(ttl)
            .unwrap_or(DateTime::<Utc>::MAX_UTC)
            .to_rfc3339()
    }

    pub fn deadline_now(&self) -> String {
        self.deadline_from(Utc::now())
    }
}

fn validate_instance_id(value: String) -> Result<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() || trimmed.len() > 255 || trimmed.chars().any(char::is_control) {
        return Err(crate::utils::error::IronCrewError::Validation(
            "IRONCREW_INSTANCE_ID must be 1-255 printable characters".into(),
        ));
    }
    Ok(trimmed.to_string())
}

/// Pluggable storage backend for run records and persistent sessions
/// (conversations and dialogs).
#[async_trait]
pub trait StateStore: Send + Sync {
    // ─── Run history ────────────────────────────────────────────────────────

    /// Called when a run starts. Writes a RunRecord with status=Running,
    /// empty task_results, finished_at="", duration_ms=0, total_tokens=0,
    /// cached_tokens=0. Returns the generated run_id (or `intent.suggested_id`
    /// if `Some` — used by the HTTP handler to pre-allocate an id before
    /// the flow runs so SSE subscribers can join mid-flight).
    async fn save_run_intent(&self, intent: RunIntent) -> Result<String>;

    /// Called when a run completes (success, partial failure, or hard
    /// failure). Atomically transitions an owned in-flight record (Running or
    /// WaitingForInput) to a terminal state. The first terminal writer wins;
    /// later writers receive `AlreadyTerminal` without replacing its payload.
    async fn update_run_completion(
        &self,
        run_id: &str,
        completion: RunCompletion,
    ) -> Result<RunTransition>;

    /// Flip an in-flight run between `Running` and `WaitingForInput` (both
    /// directions — `crew:ask_human()` suspends and resumes). Narrow by
    /// design: touches only the status column, never task_results or
    /// finished_at. Returns an error if the run_id doesn't exist or is
    /// already terminal, so a completed run can never be dragged back to an
    /// in-flight state.
    async fn update_run_status(&self, run_id: &str, status: RunStatus) -> Result<()>;

    /// Stable identity used to own run records created through this handle.
    fn instance_id(&self) -> &str;

    /// Configured duration of an ownership lease. Heartbeat loops should run
    /// at least three times within this interval.
    fn run_lease_ttl(&self) -> Duration;

    /// Extend every in-flight run owned by this instance. Returns the number
    /// of leases refreshed.
    async fn heartbeat_owned_runs(&self) -> Result<usize>;

    /// Minimal backend round-trip used by readiness checks.
    async fn health_check(&self) -> Result<()>;

    /// Atomically mark only expired (or legacy, unleased) in-flight records as
    /// abandoned. A healthy run owned by another instance is never touched.
    async fn reconcile_abandoned_runs(&self, now: &str) -> Result<usize>;

    async fn get_run(&self, run_id: &str) -> Result<RunRecord>;

    /// Paginated, metadata-only list view. Returns summaries without
    /// `task_results`, so clients can list hundreds of runs cheaply and fetch
    /// full records on demand via `get_run`.
    ///
    /// `limit` caps the number of rows returned (0 = unlimited).
    /// `offset` skips the first N rows (0 = start from the newest).
    /// `filter` selects runs matching status, tag, and/or since timestamp.
    async fn list_runs_summary(
        &self,
        filter: &ListRunsFilter,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<RunSummary>>;

    /// Count of runs matching `filter`. Paired with `list_runs_summary` to
    /// provide `total` in paginated API responses.
    async fn count_runs(&self, filter: &ListRunsFilter) -> Result<u64>;

    async fn delete_run(&self, run_id: &str) -> Result<()>;

    // Durable request idempotency.
    #[allow(dead_code)] // retained for downstream StateStore API compatibility
    async fn lookup_idempotency(
        &self,
        key_hash: &str,
        request_fingerprint: &str,
        now: &str,
    ) -> Result<IdempotencyLookup> {
        self.lookup_idempotency_for_principal(
            &PrincipalId::legacy(),
            key_hash,
            request_fingerprint,
            now,
        )
        .await
    }

    async fn lookup_idempotency_for_principal(
        &self,
        principal_id: &PrincipalId,
        key_hash: &str,
        request_fingerprint: &str,
        now: &str,
    ) -> Result<IdempotencyLookup>;

    #[allow(dead_code)] // retained for downstream StateStore API compatibility
    async fn claim_idempotency(
        &self,
        claim: IdempotencyClaim,
        max_records: usize,
        prune_batch: usize,
    ) -> Result<IdempotencyClaimOutcome> {
        let max_response_bytes = claim.max_total_response_bytes;
        self.claim_idempotency_with_limits(
            claim,
            IdempotencyLimits {
                global_max_records: max_records,
                principal_max_records: max_records,
                principal_max_in_flight: max_records,
                global_max_response_bytes: max_response_bytes,
                principal_max_response_bytes: max_response_bytes,
                prune_batch,
            },
        )
        .await
    }

    async fn claim_idempotency_with_limits(
        &self,
        claim: IdempotencyClaim,
        limits: IdempotencyLimits,
    ) -> Result<IdempotencyClaimOutcome>;

    /// Extend an in-flight claim. `true` means this attempt still owns the
    /// claim or has already completed it. `false` means it is missing or
    /// indeterminate; a different attempt is reported as `Conflict`.
    async fn heartbeat_idempotency(
        &self,
        key_hash: &str,
        attempt_id: &str,
        new_lease_expires_at: &str,
    ) -> Result<bool>;

    /// Atomically renew a keyed HTTP run's operation ledger and matching run
    /// lease. `Owned` requires both in-flight fences to belong to this exact
    /// attempt. A terminal run is returned separately so the API can preserve
    /// the winning result while stopping any remaining Lua work.
    async fn heartbeat_idempotent_run(
        &self,
        run_id: &str,
        key_hash: &str,
        attempt_id: &str,
        new_lease_expires_at: &str,
    ) -> Result<RunFenceHeartbeat>;

    #[allow(dead_code)] // retained for downstream StateStore API compatibility
    async fn complete_idempotency(
        &self,
        completion: IdempotencyCompletion,
        max_total_response_bytes: usize,
    ) -> Result<IdempotencyCompletionOutcome> {
        self.complete_idempotency_with_limits(
            completion,
            IdempotencyLimits {
                global_max_records: usize::MAX,
                principal_max_records: usize::MAX,
                principal_max_in_flight: usize::MAX,
                global_max_response_bytes: max_total_response_bytes,
                principal_max_response_bytes: max_total_response_bytes,
                prune_batch: 1,
            },
        )
        .await
    }

    async fn complete_idempotency_with_limits(
        &self,
        completion: IdempotencyCompletion,
        limits: IdempotencyLimits,
    ) -> Result<IdempotencyCompletionOutcome>;

    #[allow(dead_code)] // retained for downstream StateStore API compatibility
    async fn commit_conversation_idempotency(
        &self,
        completion: IdempotencyCompletion,
        conversation: &ConversationRecord,
        max_total_response_bytes: usize,
    ) -> Result<ConversationIdempotencyCommit> {
        self.commit_conversation_idempotency_with_limits(
            completion,
            conversation,
            IdempotencyLimits {
                global_max_records: usize::MAX,
                principal_max_records: usize::MAX,
                principal_max_in_flight: usize::MAX,
                global_max_response_bytes: max_total_response_bytes,
                principal_max_response_bytes: max_total_response_bytes,
                prune_batch: 1,
            },
        )
        .await
    }

    async fn commit_conversation_idempotency_with_limits(
        &self,
        completion: IdempotencyCompletion,
        conversation: &ConversationRecord,
        limits: IdempotencyLimits,
    ) -> Result<ConversationIdempotencyCommit>;

    async fn mark_idempotency_indeterminate(
        &self,
        key_hash: &str,
        attempt_id: &str,
        completed_at: &str,
        expires_at: &str,
    ) -> Result<bool>;

    async fn release_idempotency(&self, key_hash: &str, attempt_id: &str) -> Result<bool>;

    async fn prune_idempotency(&self, now: &str, limit: usize) -> Result<usize>;

    /// Low-cardinality resource snapshot for the authenticated caller plus
    /// global/high-water usage. Implementations must never return principal
    /// identifiers or raw idempotency keys.
    async fn idempotency_usage(
        &self,
        principal_id: &PrincipalId,
        limits: IdempotencyLimits,
    ) -> Result<IdempotencyUsage>;

    // ─── Persistent sessions ────────────────────────────────────────────────

    /// Insert or revision-guarded update a conversation record. The returned
    /// value is the new revision and must be supplied on the next save. The
    /// `(flow_path, id)` pair is the
    /// effective unique key — a legacy record with `flow_path = None` is
    /// only reachable when the caller also passes `None` (by convention, a
    /// global-scope lookup, as used by the `ironcrew inspect` CLI).
    async fn save_conversation(&self, record: &ConversationRecord) -> Result<u64>;
    /// Look up a conversation by `(flow_path, id)`. Returns `Ok(None)` when
    /// no record matches — which is how `crew:conversation({id = ...})`
    /// distinguishes a fresh session from a resumed one. When `flow_path`
    /// is `Some(..)`, the query is strictly scoped: a record belonging to a
    /// different flow (or to no flow, `flow_path = NULL`) is invisible.
    /// When `flow_path` is `None`, any matching `id` is returned — only
    /// use this form for admin paths that are not tied to a specific flow.
    async fn get_conversation(
        &self,
        flow_path: Option<&str>,
        id: &str,
    ) -> Result<Option<ConversationRecord>>;
    /// Delete a conversation scoped by `(flow_path, id)`. Same semantics as
    /// `get_conversation`: a delete with `flow_path = Some(..)` will not
    /// touch records belonging to another flow.
    async fn delete_conversation(&self, flow_path: Option<&str>, id: &str) -> Result<()>;

    /// Paginated list of conversation summaries, newest first by updated_at.
    /// When `flow_path` is `Some`, only records whose `flow_path` matches are
    /// returned. When `None`, all records are returned regardless of flow.
    async fn list_conversations(
        &self,
        flow_path: Option<&str>,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<ConversationSummary>>;

    /// Count of conversations matching the flow filter.
    async fn count_conversations(&self, flow_path: Option<&str>) -> Result<u64>;

    /// Insert or revision-guarded update a dialog state record. Returns the
    /// new revision; stale snapshots fail with `IronCrewError::Conflict`.
    /// Keyed by `(flow_path, id)` — same scoping rules as conversations.
    async fn save_dialog_state(&self, record: &DialogStateRecord) -> Result<u64>;
    /// Look up a dialog by `(flow_path, id)`. Returns `Ok(None)` when no
    /// record matches. Flow-scope semantics match `get_conversation`:
    /// `Some(path)` requires an exact flow match (legacy records with
    /// `flow_path = None` are invisible); `None` is the global admin
    /// lookup.
    async fn get_dialog_state(
        &self,
        flow_path: Option<&str>,
        id: &str,
    ) -> Result<Option<DialogStateRecord>>;
    /// Delete a dialog by `(flow_path, id)`. Never touches another
    /// flow's record when `flow_path` is `Some(..)`.
    async fn delete_dialog_state(&self, flow_path: Option<&str>, id: &str) -> Result<()>;

    // ─── Audit log ──────────────────────────────────────────────────────────

    /// Append an audit event. The backend generates a UUID v4 and
    /// writes it into the event's `id` field. Returns the generated id.
    async fn save_audit_event(&self, event: &AuditEvent) -> Result<String>;

    /// Paginated list of audit events matching the filter, sorted
    /// newest-first by `timestamp`. `limit = 0` means unlimited.
    async fn list_audit_events(
        &self,
        filter: &AuditFilter,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<AuditEvent>>;

    /// Count of events matching the filter. Paired with
    /// `list_audit_events` to provide `total` in paginated responses.
    async fn count_audit_events(&self, filter: &AuditFilter) -> Result<u64>;
}

/// Create a StateStore based on environment configuration.
///
/// `IRONCREW_STORE=json` (local default) — JSON files in the given directory
/// `IRONCREW_STORE=sqlite` — SQLite database
/// `IRONCREW_STORE=postgres` — PostgreSQL 15+ (requires `postgres` feature)
/// `IRONCREW_STORE_PATH=<path>` — path for SQLite db (default: `<default_dir>/ironcrew.db`)
/// `DATABASE_URL=postgres://...` — PostgreSQL connection string
/// `IRONCREW_PG_TABLE_PREFIX=prefix_` — table prefix for shared databases
/// `IRONCREW_DB_POOL_SIZE=10` — max PostgreSQL connections in the pool
/// `IRONCREW_DB_CONNECT_RETRIES=10` — retries (after the first attempt) when the
///   database is unreachable at startup, with exponential backoff
/// `IRONCREW_DB_CONNECT_BACKOFF_MS=1000` — base backoff between connect retries
/// `IRONCREW_INSTANCE_ID=<unique runtime id>` — optional process/pod identity
/// `IRONCREW_RUN_LEASE_TTL_SECONDS=60` — stale-run ownership timeout
///
/// Returns an `Arc` so the same instance can be shared across the crew's
/// `run()`, `conversation()`, and `dialog()` call paths without re-opening
/// the underlying connection/pool.
pub async fn create_store(
    default_dir: std::path::PathBuf,
) -> Result<std::sync::Arc<dyn StateStore>> {
    let store_type = match std::env::var("IRONCREW_STORE") {
        Ok(value) => value,
        Err(std::env::VarError::NotPresent) => "json".into(),
        Err(std::env::VarError::NotUnicode(_)) => {
            return Err(crate::utils::error::IronCrewError::Validation(
                "IRONCREW_STORE must contain valid UTF-8".into(),
            ));
        }
    };

    match store_type.to_lowercase().as_str() {
        "json" => Ok(std::sync::Arc::new(super::run_history::JsonFileStore::new(
            default_dir,
        )?)),
        "sqlite" => {
            let db_path = std::env::var("IRONCREW_STORE_PATH")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|_| default_dir.join("ironcrew.db"));
            if let Some(parent) = db_path.parent() {
                std::fs::create_dir_all(parent)?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let _ =
                        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
                }
            }
            Ok(std::sync::Arc::new(super::sqlite_store::SqliteStore::new(
                db_path,
            )?))
        }
        #[cfg(feature = "postgres")]
        "postgres" | "postgresql" => {
            let database_url = std::env::var("DATABASE_URL").map_err(|_| {
                crate::utils::error::IronCrewError::Validation(
                    "IRONCREW_STORE=postgres requires DATABASE_URL env var".into(),
                )
            })?;
            let table_prefix = std::env::var("IRONCREW_PG_TABLE_PREFIX").unwrap_or_default();
            let store =
                super::postgres_store::PostgresStore::new(&database_url, &table_prefix).await?;
            Ok(std::sync::Arc::new(store))
        }
        #[cfg(not(feature = "postgres"))]
        "postgres" | "postgresql" => Err(crate::utils::error::IronCrewError::Validation(
            "PostgreSQL backend requires building with --features postgres".into(),
        )),
        other => Err(crate::utils::error::IronCrewError::Validation(format!(
            "Unknown IRONCREW_STORE value '{other}'; expected json, sqlite, or postgres"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_lease_config_validates_identity_and_deadline() {
        assert!(RunLeaseConfig::new("", Duration::from_secs(60)).is_err());
        assert!(RunLeaseConfig::new("bad\nidentity", Duration::from_secs(60)).is_err());
        assert!(RunLeaseConfig::new("pod-a", Duration::ZERO).is_err());
        assert!(RunLeaseConfig::new("pod-a", Duration::from_millis(999)).is_err());

        let config = RunLeaseConfig::new(" pod-a ", Duration::from_secs(60)).unwrap();
        assert_eq!(config.instance_id(), "pod-a");
        let now = DateTime::parse_from_rfc3339("2026-07-18T10:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(config.deadline_from(now), "2026-07-18T10:01:00+00:00");
    }
}
