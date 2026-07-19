//! Durable HTTP request-idempotency records shared by every state backend.
//!
//! Raw `Idempotency-Key` values never reach this layer. The API hashes them
//! before persistence and stores only a versioned request fingerprint.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::engine::run_history::RunStatus;
use crate::utils::error::{IronCrewError, Result};

pub const RUN_OPERATION: &str = "flow.run";
pub const CONVERSATION_MESSAGE_OPERATION: &str = "conversation.message";
pub const MAX_IDEMPOTENCY_SCOPE_BYTES: usize = 512;
pub const MAX_IDEMPOTENCY_RESOURCE_BYTES: usize = 128;
pub const MAX_IDEMPOTENCY_TTL_SECONDS: u64 = 30 * 24 * 60 * 60;

/// Opaque server-issued identity used for admission and durable quota
/// accounting. It is derived from an operator-controlled principal label,
/// never from a bearer token, IP address, or caller-supplied audit header.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PrincipalId(String);

impl PrincipalId {
    /// Preserve ledgers created before principal-aware idempotency. Legacy
    /// static-token and unauthenticated local modes both use this identity, so
    /// an upgrade does not silently make retained keys executable again.
    pub fn legacy() -> Self {
        Self::from_domain_value(b"ironcrew:principal:legacy:v1", b"default")
    }

    pub fn anonymous() -> Self {
        // Anonymous and authenticated modes cannot coexist in one server.
        // Sharing this migration identity preserves pre-upgrade local ledgers.
        Self::legacy()
    }

    pub fn from_label(label: &str) -> Self {
        Self::from_domain_value(b"ironcrew:principal:named:v1", label.as_bytes())
    }

    pub fn from_digest(value: String) -> Result<Self> {
        let principal = Self(value);
        principal.validate()?;
        Ok(principal)
    }

    fn from_domain_value(domain: &[u8], value: &[u8]) -> Self {
        let mut digest = Sha256::new();
        digest.update(domain);
        digest.update([0]);
        digest.update(value);
        let digest = digest.finalize();
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut encoded = String::with_capacity(64);
        for byte in digest {
            encoded.push(HEX[usize::from(byte >> 4)] as char);
            encoded.push(HEX[usize::from(byte & 0x0f)] as char);
        }
        Self(encoded)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn validate(&self) -> Result<()> {
        validate_digest("principal id", &self.0)
    }
}

impl Default for PrincipalId {
    fn default() -> Self {
        Self::legacy()
    }
}

impl AsRef<str> for PrincipalId {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

/// Immutable resource policy supplied to each atomic ledger mutation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IdempotencyLimits {
    pub global_max_records: usize,
    pub principal_max_records: usize,
    pub principal_max_in_flight: usize,
    pub global_max_response_bytes: usize,
    pub principal_max_response_bytes: usize,
    pub prune_batch: usize,
}

impl IdempotencyLimits {
    pub fn validate(&self) -> Result<()> {
        if self.global_max_records == 0
            || self.principal_max_records == 0
            || self.principal_max_in_flight == 0
            || self.global_max_response_bytes == 0
            || self.principal_max_response_bytes == 0
            || self.prune_batch == 0
        {
            return Err(IronCrewError::Validation(
                "Idempotency limits must all be positive".into(),
            ));
        }
        if self.principal_max_records > self.global_max_records {
            return Err(IronCrewError::Validation(
                "Per-principal idempotency record limit cannot exceed the global limit".into(),
            ));
        }
        if self.principal_max_in_flight > self.principal_max_records {
            return Err(IronCrewError::Validation(
                "Per-principal in-flight idempotency limit cannot exceed its record limit".into(),
            ));
        }
        if self.principal_max_response_bytes > self.global_max_response_bytes {
            return Err(IronCrewError::Validation(
                "Per-principal idempotency response-byte limit cannot exceed the global limit"
                    .into(),
            ));
        }
        Ok(())
    }
}

/// Low-cardinality ledger snapshot returned to the protected metrics route.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IdempotencyUsage {
    pub global_records: usize,
    pub global_in_flight: usize,
    pub global_response_bytes: usize,
    pub principal_records: usize,
    pub principal_in_flight: usize,
    pub principal_response_bytes: usize,
    pub principal_count: usize,
    pub max_principal_records: usize,
    pub max_principal_in_flight: usize,
    pub max_principal_response_bytes: usize,
    pub principals_at_or_above_80_percent: usize,
    pub principals_at_or_above_90_percent: usize,
    pub principals_at_or_above_100_percent: usize,
}

/// Result of atomically renewing a keyed HTTP run's two durable ownership
/// fences: its idempotency ledger row and its in-flight run record.
///
/// A terminal run is distinguished from a lost fence because `crew:run()`
/// normally persists its terminal record just before the Lua worker returns.
/// The API monitor must stop any remaining Lua work in both cases, but only a
/// genuinely lost fence makes the request outcome indeterminate.
#[derive(Debug, Clone, PartialEq)]
pub enum RunFenceHeartbeat {
    Owned,
    Terminal(RunStatus),
    Lost,
}

/// Process-local notification that the run intent exists and its linked
/// durable idempotency claim is publishable. SQL backends cross that boundary
/// atomically; the single-process JSON backend performs both writes under its
/// shared lock and withholds this signal until both have succeeded.
#[derive(Debug, Clone)]
pub struct RunIntentSignal {
    sender: tokio::sync::watch::Sender<bool>,
    key_hash: String,
    principal_id: PrincipalId,
    request_fingerprint: String,
    attempt_id: String,
}

impl RunIntentSignal {
    pub fn channel(
        key_hash: String,
        principal_id: PrincipalId,
        request_fingerprint: String,
        attempt_id: String,
    ) -> (Self, tokio::sync::watch::Receiver<bool>) {
        let (sender, receiver) = tokio::sync::watch::channel(false);
        (
            Self {
                sender,
                key_hash,
                principal_id,
                request_fingerprint,
                attempt_id,
            },
            receiver,
        )
    }

    pub fn notify(&self) {
        let _ = self.sender.send(true);
    }

    pub fn lookup_identity(&self) -> (&PrincipalId, &str, &str) {
        (
            &self.principal_id,
            &self.key_hash,
            &self.request_fingerprint,
        )
    }

    pub fn matches_running(&self, record: &IdempotencyRecord, run_id: &str) -> bool {
        record.attempt_id == self.attempt_id
            && record.operation == RUN_OPERATION
            && record.resource_id == run_id
            && record.state == IdempotencyState::Running
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IdempotencyState {
    Claimed,
    Running,
    Completed,
    Indeterminate,
}

impl IdempotencyState {
    pub fn is_in_flight(self) -> bool {
        matches!(self, Self::Claimed | Self::Running)
    }

    pub fn is_terminal(self) -> bool {
        !self.is_in_flight()
    }
}

impl fmt::Display for IdempotencyState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Claimed => f.write_str("claimed"),
            Self::Running => f.write_str("running"),
            Self::Completed => f.write_str("completed"),
            Self::Indeterminate => f.write_str("indeterminate"),
        }
    }
}

impl FromStr for IdempotencyState {
    type Err = IronCrewError;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "claimed" => Ok(Self::Claimed),
            "running" => Ok(Self::Running),
            "completed" => Ok(Self::Completed),
            "indeterminate" => Ok(Self::Indeterminate),
            _ => Err(IronCrewError::Validation(format!(
                "Unknown idempotency state '{value}'"
            ))),
        }
    }
}

/// Complete durable representation of one idempotency key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdempotencyRecord {
    pub key_hash: String,
    /// Trusted quota owner. Older JSON ledgers adopt the legacy deployment
    /// principal during deserialization.
    #[serde(default = "PrincipalId::legacy")]
    pub principal_id: PrincipalId,
    pub request_fingerprint: String,
    pub operation: String,
    pub scope: String,
    pub resource_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exclusive_scope: Option<String>,
    pub attempt_id: String,
    pub owner_instance_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_revision: Option<u64>,
    pub state: IdempotencyState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_status: Option<u16>,
    /// Compact JSON response. `None` on a completed record is an intentional
    /// non-replayable tombstone used when per-record or aggregate storage caps
    /// would otherwise be exceeded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_body: Option<String>,
    pub lease_expires_at: String,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
    pub ttl_seconds: u64,
}

impl IdempotencyRecord {
    pub fn validate(&self) -> Result<()> {
        validate_digest("idempotency key hash", &self.key_hash)?;
        self.principal_id.validate()?;
        validate_digest("request fingerprint", &self.request_fingerprint)?;
        validate_operation(&self.operation)?;
        validate_bounded_printable(
            "idempotency scope",
            &self.scope,
            MAX_IDEMPOTENCY_SCOPE_BYTES,
        )?;
        validate_bounded_printable(
            "idempotency resource id",
            &self.resource_id,
            MAX_IDEMPOTENCY_RESOURCE_BYTES,
        )?;
        if let Some(scope) = self.exclusive_scope.as_deref() {
            validate_bounded_printable(
                "idempotency exclusive scope",
                scope,
                MAX_IDEMPOTENCY_SCOPE_BYTES,
            )?;
        }
        validate_bounded_printable("idempotency attempt id", &self.attempt_id, 128)?;
        validate_bounded_printable(
            "idempotency owner instance id",
            &self.owner_instance_id,
            255,
        )?;
        if self.operation == CONVERSATION_MESSAGE_OPERATION && self.base_revision.is_none() {
            return Err(IronCrewError::Validation(
                "Conversation idempotency records require a base revision".into(),
            ));
        }
        if self.state.is_in_flight() || !self.lease_expires_at.is_empty() {
            validate_timestamp("idempotency lease expiry", &self.lease_expires_at)?;
        }
        validate_timestamp("idempotency creation time", &self.created_at)?;
        validate_timestamp("idempotency update time", &self.updated_at)?;
        if let Some(value) = self.completed_at.as_deref() {
            validate_timestamp("idempotency completion time", value)?;
        }
        if let Some(value) = self.expires_at.as_deref() {
            validate_timestamp("idempotency retention expiry", value)?;
        }
        if self.ttl_seconds == 0 || self.ttl_seconds > MAX_IDEMPOTENCY_TTL_SECONDS {
            return Err(IronCrewError::Validation(format!(
                "Idempotency TTL must be 1..={MAX_IDEMPOTENCY_TTL_SECONDS} seconds"
            )));
        }
        if self.response_body.is_some() && self.response_status.is_none() {
            return Err(IronCrewError::Validation(
                "Stored idempotency response body has no HTTP status".into(),
            ));
        }
        if self.state.is_in_flight() && self.expires_at.is_some() {
            return Err(IronCrewError::Validation(
                "In-flight idempotency records must not have a retention expiry".into(),
            ));
        }
        if self.state.is_terminal() && (self.completed_at.is_none() || self.expires_at.is_none()) {
            return Err(IronCrewError::Validation(
                "Terminal idempotency records require completion and retention timestamps".into(),
            ));
        }
        Ok(())
    }

    pub fn replayable(&self) -> bool {
        matches!(
            self.state,
            IdempotencyState::Running | IdempotencyState::Completed
        ) && self.response_status.is_some()
            && self.response_body.is_some()
    }
}

/// New durable claim. Stores fill `state = claimed` exactly once.
#[derive(Debug, Clone)]
pub struct IdempotencyClaim {
    pub key_hash: String,
    pub principal_id: PrincipalId,
    /// Optional hash of a prior indeterminate key whose conversation-scope
    /// hazard the caller explicitly acknowledges. This is claim input only;
    /// it is never copied into the durable record.
    pub recovery_key_hash: Option<String>,
    pub request_fingerprint: String,
    pub operation: String,
    pub scope: String,
    pub resource_id: String,
    pub exclusive_scope: Option<String>,
    pub attempt_id: String,
    pub owner_instance_id: String,
    pub base_revision: Option<u64>,
    pub response_status: Option<u16>,
    pub response_body: Option<String>,
    /// Backwards-compatible global body budget used by the legacy
    /// `StateStore::claim_idempotency` wrapper. Principal-aware HTTP callers
    /// supply the complete `IdempotencyLimits` policy instead.
    pub max_total_response_bytes: usize,
    pub lease_expires_at: String,
    pub created_at: String,
    pub ttl_seconds: u64,
}

impl IdempotencyClaim {
    pub fn to_record(&self) -> IdempotencyRecord {
        IdempotencyRecord {
            key_hash: self.key_hash.clone(),
            principal_id: self.principal_id.clone(),
            request_fingerprint: self.request_fingerprint.clone(),
            operation: self.operation.clone(),
            scope: self.scope.clone(),
            resource_id: self.resource_id.clone(),
            exclusive_scope: self.exclusive_scope.clone(),
            attempt_id: self.attempt_id.clone(),
            owner_instance_id: self.owner_instance_id.clone(),
            base_revision: self.base_revision,
            state: IdempotencyState::Claimed,
            response_status: self.response_status,
            response_body: self.response_body.clone(),
            lease_expires_at: self.lease_expires_at.clone(),
            created_at: self.created_at.clone(),
            updated_at: self.created_at.clone(),
            completed_at: None,
            expires_at: None,
            ttl_seconds: self.ttl_seconds,
        }
    }

    pub fn validate(&self) -> Result<()> {
        self.to_record().validate()?;
        if self.max_total_response_bytes == 0 {
            return Err(IronCrewError::Validation(
                "Idempotency response-byte budget must be positive".into(),
            ));
        }
        if let Some(recovery_key_hash) = self.recovery_key_hash.as_deref() {
            validate_digest("idempotency recovery key hash", recovery_key_hash)?;
            if self.operation != CONVERSATION_MESSAGE_OPERATION
                || self.exclusive_scope.is_none()
                || recovery_key_hash == self.key_hash
            {
                return Err(IronCrewError::Validation(
                    "An idempotency recovery key must name a different conversation-message key"
                        .into(),
                ));
            }
        }
        Ok(())
    }
}

/// Terminal response for a previously claimed operation.
#[derive(Debug, Clone)]
pub struct IdempotencyCompletion {
    pub key_hash: String,
    pub principal_id: PrincipalId,
    pub request_fingerprint: String,
    pub attempt_id: String,
    pub owner_instance_id: String,
    pub response_status: u16,
    pub response_body: Option<String>,
    pub completed_at: String,
    pub expires_at: String,
}

impl IdempotencyCompletion {
    pub fn validate(&self) -> Result<()> {
        validate_digest("idempotency key hash", &self.key_hash)?;
        self.principal_id.validate()?;
        validate_digest("request fingerprint", &self.request_fingerprint)?;
        validate_bounded_printable("idempotency attempt id", &self.attempt_id, 128)?;
        validate_bounded_printable(
            "idempotency owner instance id",
            &self.owner_instance_id,
            255,
        )?;
        if !(100..=599).contains(&self.response_status) {
            return Err(IronCrewError::Validation(
                "Idempotency completion has an invalid HTTP status".into(),
            ));
        }
        validate_timestamp("idempotency completion time", &self.completed_at)?;
        validate_timestamp("idempotency retention expiry", &self.expires_at)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdempotencyLookup {
    Miss,
    Replay(IdempotencyRecord),
    InProgress(IdempotencyRecord),
    Indeterminate(IdempotencyRecord),
    Conflict,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdempotencyClaimOutcome {
    Claimed(IdempotencyRecord),
    Replay(IdempotencyRecord),
    InProgress(IdempotencyRecord),
    Indeterminate(IdempotencyRecord),
    Conflict,
    Busy,
    QuotaExceeded {
        scope: IdempotencyQuotaScope,
        resource: IdempotencyQuotaResource,
        retry_after_seconds: u64,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdempotencyQuotaScope {
    Global,
    Principal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdempotencyQuotaResource {
    Records,
    InFlight,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IdempotencyCompletionOutcome {
    pub replayable: bool,
    pub already_completed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConversationIdempotencyCommit {
    pub revision: u64,
    pub replayable: bool,
    pub already_completed: bool,
}

pub fn validate_digest(label: &str, value: &str) -> Result<()> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(IronCrewError::Validation(format!(
            "{label} must be a lowercase SHA-256 digest"
        )));
    }
    Ok(())
}

fn validate_operation(value: &str) -> Result<()> {
    if !matches!(value, RUN_OPERATION | CONVERSATION_MESSAGE_OPERATION) {
        return Err(IronCrewError::Validation(format!(
            "Unsupported idempotency operation '{value}'"
        )));
    }
    Ok(())
}

fn validate_bounded_printable(label: &str, value: &str, max_bytes: usize) -> Result<()> {
    if value.is_empty()
        || value.len() > max_bytes
        || !value
            .bytes()
            .all(|byte| byte.is_ascii() && !byte.is_ascii_control())
    {
        return Err(IronCrewError::Validation(format!(
            "{label} must be 1..={max_bytes} printable ASCII bytes"
        )));
    }
    Ok(())
}

fn validate_timestamp(label: &str, value: &str) -> Result<()> {
    chrono::DateTime::parse_from_rfc3339(value).map_err(|error| {
        IronCrewError::Validation(format!("{label} is not valid RFC3339: {error}"))
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record() -> IdempotencyRecord {
        IdempotencyRecord {
            key_hash: "a".repeat(64),
            principal_id: PrincipalId::legacy(),
            request_fingerprint: "b".repeat(64),
            operation: RUN_OPERATION.into(),
            scope: "flow-a".into(),
            resource_id: "run-1".into(),
            exclusive_scope: None,
            attempt_id: "attempt-1".into(),
            owner_instance_id: "pod-1".into(),
            base_revision: None,
            state: IdempotencyState::Claimed,
            response_status: Some(200),
            response_body: Some("{\"run_id\":\"run-1\"}".into()),
            lease_expires_at: "2026-07-19T12:01:00Z".into(),
            created_at: "2026-07-19T12:00:00Z".into(),
            updated_at: "2026-07-19T12:00:00Z".into(),
            completed_at: None,
            expires_at: None,
            ttl_seconds: 86_400,
        }
    }

    #[test]
    fn validates_well_formed_record_and_rejects_corrupt_state() {
        assert!(record().validate().is_ok());
        let mut invalid = record();
        invalid.key_hash = "RAW-SECRET-KEY".into();
        assert!(invalid.validate().is_err());
        let mut invalid = record();
        invalid.state = IdempotencyState::Completed;
        assert!(invalid.validate().is_err());
    }

    #[test]
    fn state_strings_round_trip() {
        for state in [
            IdempotencyState::Claimed,
            IdempotencyState::Running,
            IdempotencyState::Completed,
            IdempotencyState::Indeterminate,
        ] {
            assert_eq!(
                state.to_string().parse::<IdempotencyState>().unwrap(),
                state
            );
        }
    }
}
