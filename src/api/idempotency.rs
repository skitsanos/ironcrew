//! HTTP idempotency-key parsing, request fingerprints, and bounded response
//! serialization. Raw client keys are deliberately discarded after hashing.

use std::io::{self, Write};
use std::sync::Arc;
use std::time::Duration;

use axum::http::{HeaderMap, HeaderName, HeaderValue};
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::utils::error::{IronCrewError, Result};

use crate::engine::idempotency::{IdempotencyLimits, PrincipalId, RunFenceHeartbeat};
use crate::engine::store::StateStore;

pub const IDEMPOTENCY_KEY_HEADER: HeaderName = HeaderName::from_static("idempotency-key");
pub const IDEMPOTENCY_RECOVERY_KEY_HEADER: HeaderName =
    HeaderName::from_static("idempotency-recovery-key");
pub const IDEMPOTENCY_REPLAYED_HEADER: HeaderName = HeaderName::from_static("idempotency-replayed");

const DEFAULT_TTL_SECONDS: u64 = 24 * 60 * 60;
const MIN_TTL_SECONDS: u64 = 60;
const MAX_TTL_SECONDS: u64 = 30 * 24 * 60 * 60;
const DEFAULT_MAX_RECORDS: usize = 10_000;
const HARD_MAX_RECORDS: usize = 100_000;
const DEFAULT_PRUNE_BATCH: usize = 1_000;
const HARD_PRUNE_BATCH: usize = 10_000;
const DEFAULT_MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
const HARD_MAX_RESPONSE_BYTES: usize = 64 * 1024 * 1024;
const DEFAULT_MAX_TOTAL_RESPONSE_BYTES: usize = 256 * 1024 * 1024;
const HARD_MAX_TOTAL_RESPONSE_BYTES: usize = 8 * 1024 * 1024 * 1024;

/// A fenced lease heartbeat whose worker cannot outlive the request task that
/// owns it. Consumers select on `loss_receiver()` and stop external work as
/// soon as another attempt owns the claim, or once storage errors have lasted
/// through the complete local lease window.
pub struct LeaseHeartbeat {
    task: tokio::task::JoinHandle<()>,
    loss: tokio::sync::watch::Receiver<bool>,
}

impl LeaseHeartbeat {
    pub fn spawn(
        store: Arc<dyn StateStore>,
        key_hash: String,
        attempt_id: String,
        operation: &'static str,
    ) -> Self {
        let (loss_tx, loss) = tokio::sync::watch::channel(false);
        let task = tokio::spawn(async move {
            let lease_ttl = store.run_lease_ttl();
            let heartbeat_every = (lease_ttl / 3).max(Duration::from_secs(1));
            let mut lease_deadline = tokio::time::Instant::now() + lease_ttl;
            let mut interval = tokio::time::interval(heartbeat_every);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

            loop {
                interval.tick().await;
                let deadline = chrono::Utc::now()
                    .checked_add_signed(
                        chrono::Duration::from_std(lease_ttl)
                            .unwrap_or_else(|_| chrono::Duration::seconds(60)),
                    )
                    .unwrap_or(chrono::DateTime::<chrono::Utc>::MAX_UTC)
                    .to_rfc3339();
                match tokio::time::timeout_at(
                    lease_deadline,
                    store.heartbeat_idempotency(&key_hash, &attempt_id, &deadline),
                )
                .await
                {
                    Ok(Ok(true)) => {
                        lease_deadline = tokio::time::Instant::now() + lease_ttl;
                    }
                    Ok(Ok(false)) | Ok(Err(IronCrewError::Conflict(_))) => {
                        tracing::warn!(operation, "Idempotency claim was fenced during execution");
                        let _ = loss_tx.send(true);
                        return;
                    }
                    Ok(Err(error)) => {
                        tracing::error!(operation, %error, "Failed to heartbeat idempotency claim");
                        if tokio::time::Instant::now() >= lease_deadline {
                            tracing::warn!(
                                operation,
                                "Idempotency storage remained unavailable through the lease deadline"
                            );
                            let _ = loss_tx.send(true);
                            return;
                        }
                    }
                    Err(_) => {
                        tracing::warn!(
                            operation,
                            "Idempotency heartbeat exceeded the remaining lease window"
                        );
                        let _ = loss_tx.send(true);
                        return;
                    }
                }
            }
        });
        Self { task, loss }
    }

    pub fn loss_receiver(&self) -> tokio::sync::watch::Receiver<bool> {
        self.loss.clone()
    }
}

impl Drop for LeaseHeartbeat {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub async fn wait_for_lease_loss(loss: &mut tokio::sync::watch::Receiver<bool>) {
    if *loss.borrow() {
        return;
    }
    // A closed channel means the heartbeat worker exited unexpectedly. Treat
    // that exactly like a lost fence; continuing side effects would be unsafe.
    let _ = loss.wait_for(|lost| *lost).await;
}

/// Heartbeat for an idempotent run. Unlike a conversation operation, a run
/// owns two durable fences: the operation ledger and the run record itself.
/// Backends renew and validate both atomically so the global run reconciler
/// cannot terminalize a run while its Lua worker continues side effects.
pub struct RunLeaseHeartbeat {
    task: tokio::task::JoinHandle<()>,
    outcome: tokio::sync::watch::Receiver<Option<RunFenceHeartbeat>>,
}

impl RunLeaseHeartbeat {
    pub fn spawn(
        store: Arc<dyn StateStore>,
        run_id: String,
        key_hash: String,
        attempt_id: String,
    ) -> Self {
        let (outcome_tx, outcome) = tokio::sync::watch::channel(None);
        let task = tokio::spawn(async move {
            let lease_ttl = store.run_lease_ttl();
            let heartbeat_every = (lease_ttl / 3).max(Duration::from_secs(1));
            let mut lease_deadline = tokio::time::Instant::now() + lease_ttl;
            let mut interval = tokio::time::interval(heartbeat_every);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

            loop {
                interval.tick().await;
                let deadline = chrono::Utc::now()
                    .checked_add_signed(
                        chrono::Duration::from_std(lease_ttl)
                            .unwrap_or_else(|_| chrono::Duration::seconds(60)),
                    )
                    .unwrap_or(chrono::DateTime::<chrono::Utc>::MAX_UTC)
                    .to_rfc3339();
                match tokio::time::timeout_at(
                    lease_deadline,
                    store.heartbeat_idempotent_run(&run_id, &key_hash, &attempt_id, &deadline),
                )
                .await
                {
                    Ok(Ok(RunFenceHeartbeat::Owned)) => {
                        lease_deadline = tokio::time::Instant::now() + lease_ttl;
                    }
                    Ok(Ok(outcome @ RunFenceHeartbeat::Terminal(_))) => {
                        tracing::debug!(run_id, "Run heartbeat observed a terminal run fence");
                        let _ = outcome_tx.send(Some(outcome));
                        return;
                    }
                    Ok(Ok(RunFenceHeartbeat::Lost)) | Ok(Err(IronCrewError::Conflict(_))) => {
                        tracing::warn!(run_id, "Idempotent run fence was lost during execution");
                        let _ = outcome_tx.send(Some(RunFenceHeartbeat::Lost));
                        return;
                    }
                    Ok(Err(error)) => {
                        tracing::error!(run_id, %error, "Failed to heartbeat idempotent run fence");
                        if tokio::time::Instant::now() >= lease_deadline {
                            tracing::warn!(
                                run_id,
                                "Run-fence storage remained unavailable through the lease deadline"
                            );
                            let _ = outcome_tx.send(Some(RunFenceHeartbeat::Lost));
                            return;
                        }
                    }
                    Err(_) => {
                        tracing::warn!(
                            run_id,
                            "Run-fence heartbeat exceeded the remaining lease window"
                        );
                        let _ = outcome_tx.send(Some(RunFenceHeartbeat::Lost));
                        return;
                    }
                }
            }
        });
        Self { task, outcome }
    }

    pub fn outcome_receiver(&self) -> tokio::sync::watch::Receiver<Option<RunFenceHeartbeat>> {
        self.outcome.clone()
    }
}

impl Drop for RunLeaseHeartbeat {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub async fn wait_for_run_fence_outcome(
    outcome: &mut tokio::sync::watch::Receiver<Option<RunFenceHeartbeat>>,
) -> RunFenceHeartbeat {
    if let Some(outcome) = outcome.borrow().clone() {
        return outcome;
    }
    match outcome.wait_for(Option::is_some).await {
        Ok(value) => value.clone().unwrap_or(RunFenceHeartbeat::Lost),
        Err(_) => RunFenceHeartbeat::Lost,
    }
}

#[derive(Debug, Clone)]
pub struct IdempotencyConfig {
    pub require_key: bool,
    pub ttl_seconds: u64,
    pub max_records: usize,
    pub max_records_per_principal: usize,
    pub max_in_flight_per_principal: usize,
    pub prune_batch: usize,
    pub max_response_bytes: usize,
    pub max_total_response_bytes: usize,
    pub max_total_response_bytes_per_principal: usize,
}

impl IdempotencyConfig {
    /// Parse and validate the complete idempotency resource policy once at
    /// process startup. Retention cannot be shorter than the longest admitted
    /// run plus one hour, otherwise a retry could outlive its ledger row.
    pub fn from_env(max_run_lifetime: Duration) -> Result<Self> {
        let require_key = bool_env("IRONCREW_REQUIRE_IDEMPOTENCY_KEY", false)?;
        let configured_ttl = bounded_env_u64(
            "IRONCREW_IDEMPOTENCY_TTL_SECONDS",
            DEFAULT_TTL_SECONDS,
            MIN_TTL_SECONDS,
            MAX_TTL_SECONDS,
        )?;
        let minimum_safe_ttl = max_run_lifetime
            .as_secs()
            .saturating_add(60 * 60)
            .clamp(MIN_TTL_SECONDS, MAX_TTL_SECONDS);
        if configured_ttl < minimum_safe_ttl {
            return Err(IronCrewError::Validation(format!(
                "IRONCREW_IDEMPOTENCY_TTL_SECONDS must be at least {minimum_safe_ttl} for the configured run lifetime"
            )));
        }

        let max_records = bounded_env_usize(
            "IRONCREW_IDEMPOTENCY_MAX_RECORDS",
            DEFAULT_MAX_RECORDS,
            1,
            HARD_MAX_RECORDS,
        )?;
        let max_total_response_bytes = bounded_env_usize(
            "IRONCREW_IDEMPOTENCY_MAX_TOTAL_RESPONSE_BYTES",
            DEFAULT_MAX_TOTAL_RESPONSE_BYTES,
            1,
            HARD_MAX_TOTAL_RESPONSE_BYTES,
        )?;
        let config = Self {
            require_key,
            ttl_seconds: configured_ttl,
            max_records,
            max_records_per_principal: bounded_env_usize(
                "IRONCREW_IDEMPOTENCY_MAX_RECORDS_PER_PRINCIPAL",
                max_records,
                1,
                max_records,
            )?,
            max_in_flight_per_principal: bounded_env_usize(
                "IRONCREW_IDEMPOTENCY_MAX_IN_FLIGHT_PER_PRINCIPAL",
                max_records.min(64),
                1,
                max_records,
            )?,
            prune_batch: bounded_env_usize(
                "IRONCREW_IDEMPOTENCY_PRUNE_BATCH",
                DEFAULT_PRUNE_BATCH,
                1,
                HARD_PRUNE_BATCH,
            )?,
            max_response_bytes: bounded_env_usize(
                "IRONCREW_IDEMPOTENCY_MAX_RESPONSE_BYTES",
                DEFAULT_MAX_RESPONSE_BYTES,
                1,
                HARD_MAX_RESPONSE_BYTES,
            )?,
            max_total_response_bytes,
            max_total_response_bytes_per_principal: bounded_env_usize(
                "IRONCREW_IDEMPOTENCY_MAX_TOTAL_RESPONSE_BYTES_PER_PRINCIPAL",
                max_total_response_bytes,
                1,
                max_total_response_bytes,
            )?,
        };
        config.limits().validate()?;
        Ok(config)
    }

    pub fn retention_expiry(&self, completed_at: chrono::DateTime<chrono::Utc>) -> String {
        completed_at
            .checked_add_signed(chrono::Duration::seconds(self.ttl_seconds as i64))
            .unwrap_or(chrono::DateTime::<chrono::Utc>::MAX_UTC)
            .to_rfc3339()
    }

    pub fn limits(&self) -> IdempotencyLimits {
        IdempotencyLimits {
            global_max_records: self.max_records,
            principal_max_records: self.max_records_per_principal,
            principal_max_in_flight: self.max_in_flight_per_principal,
            global_max_response_bytes: self.max_total_response_bytes,
            principal_max_response_bytes: self.max_total_response_bytes_per_principal,
            prune_batch: self.prune_batch,
        }
    }
}

impl Default for IdempotencyConfig {
    fn default() -> Self {
        Self {
            require_key: false,
            ttl_seconds: DEFAULT_TTL_SECONDS,
            max_records: DEFAULT_MAX_RECORDS,
            max_records_per_principal: DEFAULT_MAX_RECORDS,
            max_in_flight_per_principal: 64,
            prune_batch: DEFAULT_PRUNE_BATCH,
            max_response_bytes: DEFAULT_MAX_RESPONSE_BYTES,
            max_total_response_bytes: DEFAULT_MAX_TOTAL_RESPONSE_BYTES,
            max_total_response_bytes_per_principal: DEFAULT_MAX_TOTAL_RESPONSE_BYTES,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestKey {
    pub key_hash: String,
}

/// Parse exactly zero or one idempotency-key header. Only visible non-space
/// ASCII is accepted so proxies and databases cannot disagree about the key.
/// The returned value contains the SHA-256 digest only.
pub fn request_key(
    headers: &HeaderMap,
    required: bool,
    principal_id: &PrincipalId,
) -> Result<Option<RequestKey>> {
    parse_key_header(
        headers,
        &IDEMPOTENCY_KEY_HEADER,
        "Idempotency-Key",
        required,
        principal_id,
    )
}

pub fn recovery_key(headers: &HeaderMap, principal_id: &PrincipalId) -> Result<Option<RequestKey>> {
    parse_key_header(
        headers,
        &IDEMPOTENCY_RECOVERY_KEY_HEADER,
        "Idempotency-Recovery-Key",
        false,
        principal_id,
    )
}

fn parse_key_header(
    headers: &HeaderMap,
    header: &HeaderName,
    label: &str,
    required: bool,
    principal_id: &PrincipalId,
) -> Result<Option<RequestKey>> {
    let mut values = headers.get_all(header).iter();
    let Some(value) = values.next() else {
        if required {
            return Err(IronCrewError::Validation(format!(
                "{label} is required for this endpoint"
            )));
        }
        return Ok(None);
    };
    if values.next().is_some() {
        return Err(IronCrewError::Validation(format!(
            "Exactly one {label} header is allowed"
        )));
    }
    let bytes = value.as_bytes();
    if bytes.is_empty() || bytes.len() > 128 || !bytes.iter().all(|byte| (33..=126).contains(byte))
    {
        return Err(IronCrewError::Validation(format!(
            "{label} must be 1-128 visible ASCII bytes without whitespace"
        )));
    }
    // Legacy/anonymous deployments retain the original digest so an upgrade
    // cannot accidentally re-execute an existing key. Explicit named
    // principals receive separate namespaces and may safely reuse client keys.
    let key_hash = if principal_id == &PrincipalId::legacy() {
        hex_digest(bytes)
    } else {
        let mut encoder = FingerprintEncoder::new(b"ironcrew:idempotency-key:v2");
        encoder.field(principal_id.as_str().as_bytes());
        encoder.field(bytes);
        encoder.finish()
    };
    Ok(Some(RequestKey { key_hash }))
}

pub fn replay_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        IDEMPOTENCY_REPLAYED_HEADER,
        HeaderValue::from_static("true"),
    );
    headers
}

/// Versioned semantic fingerprint for `POST /flows/{flow}/run`. A missing
/// JSON body and explicit JSON `null` remain distinct.
pub fn run_fingerprint(flow: &str, body: Option<&Value>) -> String {
    let mut encoder = FingerprintEncoder::new(b"ironcrew:flow.run:v1");
    encoder.field(flow.as_bytes());
    match body {
        Some(value) => {
            encoder.field(b"body:present");
            encoder.json(value);
        }
        None => encoder.field(b"body:missing"),
    }
    encoder.finish()
}

/// Versioned fingerprint for a conversation message. `images = null`, an
/// absent field, and an empty array are deliberately equivalent because the
/// handler treats all three as a text-only turn.
pub fn conversation_message_fingerprint(
    flow: &str,
    conversation_id: &str,
    content: &str,
    images: Option<&[String]>,
) -> String {
    let mut encoder = FingerprintEncoder::new(b"ironcrew:conversation.message:v1");
    encoder.field(flow.as_bytes());
    encoder.field(conversation_id.as_bytes());
    encoder.field(content.as_bytes());
    let images = images.unwrap_or_default();
    encoder.field(&(images.len() as u64).to_be_bytes());
    for image in images {
        encoder.field(image.as_bytes());
    }
    encoder.finish()
}

pub fn run_scope(flow: &str) -> String {
    flow.to_string()
}

pub fn conversation_scope(flow: &str, conversation_id: &str) -> String {
    format!("conversation.message:{flow}:{conversation_id}")
}

/// Compactly serialize a response without ever allocating more than the
/// configured per-record limit. `None` is a durable non-replayable tombstone.
pub fn bounded_response_json<T: Serialize>(value: &T, max_bytes: usize) -> Result<Option<String>> {
    let writer = BoundedWriter::new(max_bytes);
    let mut serializer = serde_json::Serializer::new(writer);
    match value.serialize(&mut serializer) {
        Ok(()) => {
            let bytes = serializer.into_inner().bytes;
            String::from_utf8(bytes).map(Some).map_err(|error| {
                IronCrewError::Validation(format!(
                    "Serialized idempotency response was not UTF-8: {error}"
                ))
            })
        }
        Err(error) if error.is_io() => Ok(None),
        Err(error) => Err(IronCrewError::Validation(format!(
            "Failed to serialize idempotency response: {error}"
        ))),
    }
}

fn bool_env(name: &str, default: bool) -> Result<bool> {
    match std::env::var(name) {
        Err(std::env::VarError::NotPresent) => Ok(default),
        Ok(value) if value == "1" || value.eq_ignore_ascii_case("true") => Ok(true),
        Ok(value) if value == "0" || value.eq_ignore_ascii_case("false") => Ok(false),
        Ok(_) | Err(std::env::VarError::NotUnicode(_)) => Err(IronCrewError::Validation(format!(
            "{name} must be one of: 1, true, 0, false"
        ))),
    }
}

fn bounded_env_u64(name: &str, default: u64, min: u64, max: u64) -> Result<u64> {
    let value = match std::env::var(name) {
        Ok(value) => value.parse::<u64>().map_err(|_| {
            IronCrewError::Validation(format!("{name} must be an integer between {min} and {max}"))
        })?,
        Err(std::env::VarError::NotPresent) => default,
        Err(std::env::VarError::NotUnicode(_)) => {
            return Err(IronCrewError::Validation(format!(
                "{name} must contain valid UTF-8"
            )));
        }
    };
    if !(min..=max).contains(&value) {
        return Err(IronCrewError::Validation(format!(
            "{name} must be between {min} and {max}"
        )));
    }
    Ok(value)
}

fn bounded_env_usize(name: &str, default: usize, min: usize, max: usize) -> Result<usize> {
    let value = bounded_env_u64(name, default as u64, min as u64, max as u64)?;
    usize::try_from(value)
        .map_err(|_| IronCrewError::Validation(format!("{name} does not fit this platform")))
}

fn hex_digest(bytes: &[u8]) -> String {
    encode_hex(&Sha256::digest(bytes))
}

fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0x0f) as usize] as char);
    }
    encoded
}

struct FingerprintEncoder(Sha256);

impl FingerprintEncoder {
    fn new(domain: &[u8]) -> Self {
        let mut digest = Sha256::new();
        digest.update((domain.len() as u64).to_be_bytes());
        digest.update(domain);
        Self(digest)
    }

    fn field(&mut self, value: &[u8]) {
        self.0.update((value.len() as u64).to_be_bytes());
        self.0.update(value);
    }

    fn json(&mut self, value: &Value) {
        match value {
            Value::Null => self.field(b"null"),
            Value::Bool(value) => self.field(if *value { b"true" } else { b"false" }),
            Value::Number(value) => {
                self.field(b"number");
                self.field(value.to_string().as_bytes());
            }
            Value::String(value) => {
                self.field(b"string");
                self.field(value.as_bytes());
            }
            Value::Array(values) => {
                self.field(b"array");
                self.field(&(values.len() as u64).to_be_bytes());
                for value in values {
                    self.json(value);
                }
            }
            Value::Object(values) => {
                self.field(b"object");
                self.field(&(values.len() as u64).to_be_bytes());
                let mut keys = values.keys().collect::<Vec<_>>();
                keys.sort_unstable();
                for key in keys {
                    self.field(key.as_bytes());
                    self.json(&values[key]);
                }
            }
        }
    }

    fn finish(self) -> String {
        encode_hex(&self.0.finalize())
    }
}

struct BoundedWriter {
    bytes: Vec<u8>,
    limit: usize,
}

impl BoundedWriter {
    fn new(limit: usize) -> Self {
        Self {
            bytes: Vec::with_capacity(limit.min(64 * 1024)),
            limit,
        }
    }
}

impl Write for BoundedWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let new_len = self
            .bytes
            .len()
            .checked_add(buffer.len())
            .ok_or_else(|| io::Error::other("idempotency response exceeded its byte cap"))?;
        if new_len > self.limit {
            return Err(io::Error::other(
                "idempotency response exceeded its byte cap",
            ));
        }
        self.bytes.extend_from_slice(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_key_is_hashed_and_malformed_or_multiple_values_fail() {
        let principal = PrincipalId::legacy();
        let mut headers = HeaderMap::new();
        headers.insert(
            IDEMPOTENCY_KEY_HEADER,
            HeaderValue::from_static("client-key-1"),
        );
        let parsed = request_key(&headers, true, &principal).unwrap().unwrap();
        assert_eq!(parsed.key_hash.len(), 64);
        assert!(!parsed.key_hash.contains("client-key-1"));

        headers.append(IDEMPOTENCY_KEY_HEADER, HeaderValue::from_static("second"));
        assert!(request_key(&headers, false, &principal).is_err());

        let mut headers = HeaderMap::new();
        headers.insert(
            IDEMPOTENCY_KEY_HEADER,
            HeaderValue::from_static("contains space"),
        );
        assert!(request_key(&headers, false, &principal).is_err());
        assert!(request_key(&HeaderMap::new(), true, &principal).is_err());
    }

    #[test]
    fn recovery_key_uses_the_same_secret_safe_validation() {
        let principal = PrincipalId::legacy();
        let mut headers = HeaderMap::new();
        headers.insert(
            IDEMPOTENCY_RECOVERY_KEY_HEADER,
            HeaderValue::from_static("prior-message-key"),
        );
        let parsed = recovery_key(&headers, &principal).unwrap().unwrap();
        assert_eq!(parsed.key_hash.len(), 64);
        assert!(!parsed.key_hash.contains("prior-message-key"));

        headers.append(
            IDEMPOTENCY_RECOVERY_KEY_HEADER,
            HeaderValue::from_static("duplicate"),
        );
        assert!(recovery_key(&headers, &principal).is_err());
    }

    #[test]
    fn run_fingerprint_canonicalizes_object_keys_but_not_array_order() {
        let first = serde_json::json!({"b": [1, 2], "a": true});
        let reordered = serde_json::json!({"a": true, "b": [1, 2]});
        let changed_array = serde_json::json!({"a": true, "b": [2, 1]});
        assert_eq!(
            run_fingerprint("flow", Some(&first)),
            run_fingerprint("flow", Some(&reordered))
        );
        assert_ne!(
            run_fingerprint("flow", Some(&first)),
            run_fingerprint("flow", Some(&changed_array))
        );
        assert_ne!(
            run_fingerprint("flow", None),
            run_fingerprint("flow", Some(&Value::Null))
        );
    }

    #[test]
    fn absent_and_empty_message_images_have_one_fingerprint() {
        assert_eq!(
            conversation_message_fingerprint("flow", "c1", "hello", None),
            conversation_message_fingerprint("flow", "c1", "hello", Some(&[]))
        );
    }

    #[test]
    fn bounded_response_never_retains_an_oversized_body() {
        let response = serde_json::json!({"value": "x".repeat(100)});
        assert!(bounded_response_json(&response, 256).unwrap().is_some());
        assert!(bounded_response_json(&response, 8).unwrap().is_none());
    }
}
