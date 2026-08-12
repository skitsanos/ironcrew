//! Durable human-input contracts and answer encryption.
//!
//! PostgreSQL-backed runs can use these types to share pending questions
//! across replicas without persisting human answers as plaintext. Key
//! configuration is parsed once per process and answer metadata is bound into
//! AES-256-GCM additional authenticated data (AAD), so ciphertext cannot be
//! moved between runs, attempts, owners, or questions.

use std::collections::HashSet;
use std::fmt;
use std::io::Write;
use std::sync::{Arc, OnceLock};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use ring::aead::{AES_256_GCM, Aad, LessSafeKey, Nonce, UnboundKey};
use ring::rand::{SecureRandom, SystemRandom};
use serde::de::{DeserializeOwned, MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use zeroize::{Zeroize, Zeroizing};

use crate::engine::input_bridge::QuestionInfo;
use crate::utils::error::{IronCrewError, Result};

pub const HITL_ENCRYPTION_KEYS_ENV: &str = "IRONCREW_HITL_ENCRYPTION_KEYS";
pub const HITL_ACTIVE_KEY_ID_ENV: &str = "IRONCREW_HITL_ACTIVE_KEY_ID";

const KEY_BYTES: usize = 32;
const NONCE_BYTES: usize = 12;
const TAG_BYTES: usize = 16;
const FINGERPRINT_BYTES: usize = 64;
const MAX_KEYRING_JSON_BYTES: usize = 16 * 1024;
const MAX_KEYS: usize = 8;
const MAX_KEY_ID_BYTES: usize = 64;

const MAX_FLOW_BYTES: usize = 255;
const MAX_RUN_ID_BYTES: usize = 128;
const MAX_QUESTION_ID_BYTES: usize = 128;
const MAX_OWNER_INSTANCE_ID_BYTES: usize = 255;
const MAX_ATTEMPT_ID_BYTES: usize = 128;
const MAX_ASKED_AT_BYTES: usize = 64;
const HARD_MAX_TIMEOUT_SECS: u64 = 86_400;
const HARD_MAX_PROMPT_BYTES: usize = 1024 * 1024;
const HARD_MAX_CHOICES: usize = 1_000;
const HARD_MAX_CHOICES_BYTES: usize = 1024 * 1024;
const DEFAULT_MAX_ANSWER_BYTES: usize = 64 * 1024;
const HARD_MAX_ANSWER_BYTES: usize = 1024 * 1024;
// JSON control-character escaping can expand one input byte to six bytes.
const HARD_MAX_QUESTION_METADATA_BYTES: usize =
    6 * (HARD_MAX_PROMPT_BYTES + HARD_MAX_CHOICES_BYTES) + 128 * 1024;

const AAD_DOMAIN: &[u8] = b"ironcrew/hitl";
const AAD_VERSION: &[u8] = b"2";
const AAD_PURPOSE_ANSWER: &[u8] = b"answer";
const AAD_PURPOSE_QUESTION: &[u8] = b"question";

static ENV_KEYRING: OnceLock<std::result::Result<Option<HumanInputKeyring>, String>> =
    OnceLock::new();

/// Store input for registering one durable pending question.
#[derive(Clone)]
pub struct DurableHumanInputRegistration {
    pub flow: String,
    pub run_id: String,
    pub question: QuestionInfo,
    pub key_hash: String,
    pub attempt_id: String,
}

impl DurableHumanInputRegistration {
    pub fn validate(&self) -> Result<()> {
        validate_printable("flow", &self.flow, MAX_FLOW_BYTES)?;
        validate_printable("run id", &self.run_id, MAX_RUN_ID_BYTES)?;
        validate_question(&self.question)?;
        validate_fingerprint("idempotency key hash", &self.key_hash)?;
        validate_printable("attempt id", &self.attempt_id, MAX_ATTEMPT_ID_BYTES)
    }

    pub fn aad(&self, owner_instance_id: impl Into<String>) -> Result<HumanInputAad> {
        HumanInputAad::new(
            self.flow.clone(),
            self.run_id.clone(),
            self.question.question_id.clone(),
            question_digest(&self.question)?,
            owner_instance_id,
            self.key_hash.clone(),
            self.attempt_id.clone(),
        )
    }
}

impl fmt::Debug for DurableHumanInputRegistration {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DurableHumanInputRegistration")
            .field("flow", &self.flow)
            .field("run_id", &self.run_id)
            .field("question_id", &self.question.question_id)
            .field("prompt_bytes", &self.question.prompt.len())
            .field("choices", &self.question.choices.len())
            .field("key_hash", &self.key_hash)
            .field("attempt_id", &self.attempt_id)
            .finish()
    }
}

/// Public pending-question metadata plus the replica that owns the run lease.
#[derive(Clone)]
pub struct DurableHumanInputQuestion {
    pub info: QuestionInfo,
    pub owner_instance_id: String,
}

impl DurableHumanInputQuestion {
    pub fn validate(&self) -> Result<()> {
        validate_question(&self.info)?;
        validate_printable(
            "owner instance id",
            &self.owner_instance_id,
            MAX_OWNER_INSTANCE_ID_BYTES,
        )
    }
}

impl fmt::Debug for DurableHumanInputQuestion {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DurableHumanInputQuestion")
            .field("question_id", &self.info.question_id)
            .field("asked_at", &self.info.asked_at)
            .field("timeout_s", &self.info.timeout_s)
            .field("kind", &self.info.kind)
            .field("prompt_bytes", &self.info.prompt.len())
            .field("choices", &self.info.choices.len())
            .field("owner_instance_id", &self.owner_instance_id)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HumanInputRegistrationOutcome {
    Registered,
    OwnerDraining { owner_instance_id: String },
    NotDurable,
}

#[derive(Debug, Clone)]
pub enum HumanInputListOutcome {
    Shared {
        owner_instance_id: String,
        questions: Vec<DurableHumanInputQuestion>,
    },
    NotDurable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HumanInputAnswerOutcome {
    Queued { owner_instance_id: String },
    OwnerDraining { owner_instance_id: String },
    AlreadyAnswered,
    NotFound,
    NotDurable,
}

#[derive(Clone, PartialEq)]
pub enum HumanInputReadOutcome {
    Answered(Value),
    Pending,
    NotFound,
    NotDurable,
}

impl fmt::Debug for HumanInputReadOutcome {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Answered(_) => formatter.write_str("Answered(<redacted>)"),
            Self::Pending => formatter.write_str("Pending"),
            Self::NotFound => formatter.write_str("NotFound"),
            Self::NotDurable => formatter.write_str("NotDurable"),
        }
    }
}

/// Validate an answer before a store accepts it for durable delivery.
///
/// This intentionally mirrors the bridge's configured limit, while retaining
/// the same one-mebibyte hard ceiling. Stores must validate independently
/// because trait callers are not necessarily HTTP bridge callers.
pub fn validate_durable_answer(answer: &Value) -> Result<()> {
    let max_bytes = configured_max_answer_bytes();
    let mut counter = BoundedCounter {
        bytes: 0,
        max_bytes,
    };
    serde_json::to_writer(&mut counter, answer).map_err(|_| {
        IronCrewError::Validation(format!(
            "Question answer exceeds IRONCREW_ASK_HUMAN_MAX_ANSWER_BYTES ({max_bytes})"
        ))
    })
}

/// Stable digest of the complete public question contract. Besides making a
/// repeated registration idempotent only for the same semantic question, this
/// digest is authenticated in answer AAD so an old answer cannot be moved to a
/// reused question id within the same run attempt.
pub fn question_digest(question: &QuestionInfo) -> Result<String> {
    validate_question(question)?;

    struct DigestWriter(Sha256);
    impl Write for DigestWriter {
        fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
            self.0.update(buffer);
            Ok(buffer.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    let mut writer = DigestWriter(Sha256::new());
    serde_json::to_writer(&mut writer, question).map_err(|_| {
        IronCrewError::Validation("Failed to digest human-input question metadata".into())
    })?;
    Ok(encode_hex(&writer.0.finalize()))
}

/// Identifiers authenticated with a durable human-input answer.
#[derive(Clone, PartialEq, Eq)]
pub struct HumanInputAad {
    pub flow: String,
    pub run_id: String,
    pub question_id: String,
    pub question_digest: String,
    pub owner_instance_id: String,
    pub key_hash: String,
    pub attempt_id: String,
}

impl HumanInputAad {
    pub fn new(
        flow: impl Into<String>,
        run_id: impl Into<String>,
        question_id: impl Into<String>,
        question_digest: impl Into<String>,
        owner_instance_id: impl Into<String>,
        key_hash: impl Into<String>,
        attempt_id: impl Into<String>,
    ) -> Result<Self> {
        let aad = Self {
            flow: flow.into(),
            run_id: run_id.into(),
            question_id: question_id.into(),
            question_digest: question_digest.into(),
            owner_instance_id: owner_instance_id.into(),
            key_hash: key_hash.into(),
            attempt_id: attempt_id.into(),
        };
        aad.validate()?;
        Ok(aad)
    }

    pub fn validate(&self) -> Result<()> {
        validate_printable("flow", &self.flow, MAX_FLOW_BYTES)?;
        validate_printable("run id", &self.run_id, MAX_RUN_ID_BYTES)?;
        validate_printable("question id", &self.question_id, MAX_QUESTION_ID_BYTES)?;
        validate_fingerprint("question digest", &self.question_digest)?;
        validate_printable(
            "owner instance id",
            &self.owner_instance_id,
            MAX_OWNER_INSTANCE_ID_BYTES,
        )?;
        validate_fingerprint("idempotency key hash", &self.key_hash)?;
        validate_printable("attempt id", &self.attempt_id, MAX_ATTEMPT_ID_BYTES)
    }

    fn encode(&self, key_fingerprint: &str, purpose: &[u8]) -> Result<Vec<u8>> {
        self.validate()?;
        validate_fingerprint("key fingerprint", key_fingerprint)?;

        let fields = [
            AAD_DOMAIN,
            AAD_VERSION,
            purpose,
            self.flow.as_bytes(),
            self.run_id.as_bytes(),
            self.question_id.as_bytes(),
            self.question_digest.as_bytes(),
            self.owner_instance_id.as_bytes(),
            self.key_hash.as_bytes(),
            self.attempt_id.as_bytes(),
            key_fingerprint.as_bytes(),
        ];
        let capacity = fields
            .iter()
            .try_fold(0usize, |total, field| {
                total.checked_add(4)?.checked_add(field.len())
            })
            .ok_or_else(|| IronCrewError::Validation("human-input AAD is too large".into()))?;
        let mut encoded = Vec::with_capacity(capacity);
        for field in fields {
            let length = u32::try_from(field.len()).map_err(|_| {
                IronCrewError::Validation("human-input AAD field is too large".into())
            })?;
            encoded.extend_from_slice(&length.to_be_bytes());
            encoded.extend_from_slice(field);
        }
        Ok(encoded)
    }
}

impl fmt::Debug for HumanInputAad {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HumanInputAad")
            .field("flow", &self.flow)
            .field("run_id", &self.run_id)
            .field("question_id", &self.question_id)
            .field("question_digest", &self.question_digest)
            .field("owner_instance_id", &self.owner_instance_id)
            .field("key_hash", &self.key_hash)
            .field("attempt_id", &self.attempt_id)
            .finish()
    }
}

/// BYTEA-ready encrypted answer columns.
#[derive(Clone, PartialEq, Eq)]
pub struct EncryptedHumanInput {
    pub key_fingerprint: String,
    pub nonce: Vec<u8>,
    pub ciphertext: Vec<u8>,
}

impl fmt::Debug for EncryptedHumanInput {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EncryptedHumanInput")
            .field("key_fingerprint", &self.key_fingerprint)
            .field("nonce_bytes", &self.nonce.len())
            .field("ciphertext_bytes", &self.ciphertext.len())
            .finish()
    }
}

#[derive(Clone)]
pub struct HumanInputKeyring {
    inner: Arc<KeyringInner>,
}

struct KeyringInner {
    active_key_id: String,
    keys: Vec<HumanInputKey>,
}

struct HumanInputKey {
    id: String,
    fingerprint: String,
    material: [u8; KEY_BYTES],
}

impl Drop for HumanInputKey {
    fn drop(&mut self) {
        self.material.zeroize();
    }
}
impl HumanInputKeyring {
    /// Load and validate the process keyring exactly once.
    ///
    /// Both variables must be absent to disable durable answer encryption.
    /// Supplying only one, malformed JSON, an unsafe key id, or a non-256-bit
    /// key fails startup without logging key material.
    pub fn from_env() -> Result<Option<Self>> {
        match ENV_KEYRING.get_or_init(Self::read_env) {
            Ok(keyring) => Ok(keyring.clone()),
            Err(message) => Err(IronCrewError::Validation(message.clone())),
        }
    }

    /// Parse a keyring JSON object for deterministic configuration/testing.
    /// Values must be canonical standard-base64 encodings of 32-byte keys.
    pub fn from_json(keys_json: &str, active_key_id: &str) -> Result<Self> {
        Self::parse_json(keys_json, active_key_id).map_err(IronCrewError::Validation)
    }

    pub fn active_fingerprint(&self) -> &str {
        &self.active_key().fingerprint
    }

    pub(crate) fn fingerprints(&self) -> impl Iterator<Item = &str> {
        self.inner.keys.iter().map(|key| key.fingerprint.as_str())
    }

    pub fn seal_json(&self, aad: &HumanInputAad, answer: &Value) -> Result<EncryptedHumanInput> {
        self.seal_serialized_with_key(
            self.active_key(),
            aad,
            answer,
            configured_max_answer_bytes(),
            "human-input answer",
            AAD_PURPOSE_ANSWER,
        )
    }

    /// Encrypt an answer with the key that encrypted its retained question.
    ///
    /// During a staged rotation, replicas may intentionally select different
    /// active keys while every replica still carries the complete overlap
    /// keyring. Keeping the answer on the question's key guarantees that the
    /// owning process can consume it even when a newer peer accepts it.
    pub(crate) fn seal_json_for_fingerprint(
        &self,
        aad: &HumanInputAad,
        answer: &Value,
        key_fingerprint: &str,
    ) -> Result<EncryptedHumanInput> {
        let key = self.key_for_fingerprint(key_fingerprint)?;
        self.seal_serialized_with_key(
            key,
            aad,
            answer,
            configured_max_answer_bytes(),
            "human-input answer",
            AAD_PURPOSE_ANSWER,
        )
    }

    pub fn seal_question(
        &self,
        aad: &HumanInputAad,
        question: &QuestionInfo,
    ) -> Result<EncryptedHumanInput> {
        validate_question(question)?;
        if question_digest(question)? != aad.question_digest {
            return Err(IronCrewError::Validation(
                "human-input question metadata does not match its authenticated digest".into(),
            ));
        }
        self.seal_serialized_with_key(
            self.active_key(),
            aad,
            question,
            HARD_MAX_QUESTION_METADATA_BYTES,
            "human-input question metadata",
            AAD_PURPOSE_QUESTION,
        )
    }

    fn seal_serialized_with_key<T: Serialize>(
        &self,
        key: &HumanInputKey,
        aad: &HumanInputAad,
        payload: &T,
        max_plaintext_bytes: usize,
        payload_name: &str,
        purpose: &[u8],
    ) -> Result<EncryptedHumanInput> {
        let associated_data = aad.encode(&key.fingerprint, purpose)?;
        let mut plaintext = Zeroizing::new(serialize_bounded(
            payload,
            max_plaintext_bytes,
            payload_name,
        )?);
        let mut nonce_bytes = [0_u8; NONCE_BYTES];
        SystemRandom::new().fill(&mut nonce_bytes).map_err(|_| {
            IronCrewError::Validation("secure human-input nonce generation failed".into())
        })?;

        let sealing_key = aead_key(&key.material)?;
        if sealing_key
            .seal_in_place_append_tag(
                Nonce::assume_unique_for_key(nonce_bytes),
                Aad::from(associated_data),
                &mut *plaintext,
            )
            .is_err()
        {
            return Err(IronCrewError::Validation(
                "human-input payload encryption failed".into(),
            ));
        }

        Ok(EncryptedHumanInput {
            key_fingerprint: key.fingerprint.clone(),
            nonce: nonce_bytes.to_vec(),
            ciphertext: std::mem::take(&mut *plaintext),
        })
    }

    pub fn open_json(
        &self,
        aad: &HumanInputAad,
        key_fingerprint: &str,
        nonce: &[u8],
        ciphertext: &[u8],
    ) -> Result<Value> {
        self.open_serialized(
            aad,
            key_fingerprint,
            nonce,
            ciphertext,
            HARD_MAX_ANSWER_BYTES,
            AAD_PURPOSE_ANSWER,
        )
    }

    pub fn open_question(
        &self,
        aad: &HumanInputAad,
        key_fingerprint: &str,
        nonce: &[u8],
        ciphertext: &[u8],
    ) -> Result<QuestionInfo> {
        let question = self.open_serialized(
            aad,
            key_fingerprint,
            nonce,
            ciphertext,
            HARD_MAX_QUESTION_METADATA_BYTES,
            AAD_PURPOSE_QUESTION,
        )?;
        validate_question(&question)?;
        if question_digest(&question)? != aad.question_digest {
            return Err(authentication_error());
        }
        Ok(question)
    }

    fn open_serialized<T: DeserializeOwned>(
        &self,
        aad: &HumanInputAad,
        key_fingerprint: &str,
        nonce: &[u8],
        ciphertext: &[u8],
        max_plaintext_bytes: usize,
        purpose: &[u8],
    ) -> Result<T> {
        validate_fingerprint("key fingerprint", key_fingerprint)?;
        if nonce.len() != NONCE_BYTES
            || ciphertext.len() < TAG_BYTES
            || ciphertext.len() > max_plaintext_bytes + TAG_BYTES
        {
            return Err(authentication_error());
        }

        let key = self.key_for_fingerprint(key_fingerprint)?;
        let associated_data = aad.encode(key_fingerprint, purpose)?;
        let mut nonce_bytes = [0_u8; NONCE_BYTES];
        nonce_bytes.copy_from_slice(nonce);
        let opening_key = aead_key(&key.material)?;
        let mut plaintext = Zeroizing::new(ciphertext.to_vec());
        let plaintext_len = match opening_key.open_in_place(
            Nonce::assume_unique_for_key(nonce_bytes),
            Aad::from(associated_data),
            &mut plaintext,
        ) {
            Ok(opened) => opened.len(),
            Err(_) => return Err(authentication_error()),
        };
        plaintext.truncate(plaintext_len);

        let payload = match serde_json::from_slice(&plaintext) {
            Ok(payload) => payload,
            Err(_) => return Err(authentication_error()),
        };
        Ok(payload)
    }

    fn active_key(&self) -> &HumanInputKey {
        self.inner
            .keys
            .iter()
            .find(|key| key.id == self.inner.active_key_id)
            .expect("validated keyring must contain its active key")
    }

    fn key_for_fingerprint(&self, key_fingerprint: &str) -> Result<&HumanInputKey> {
        validate_fingerprint("key fingerprint", key_fingerprint)?;
        self.inner
            .keys
            .iter()
            .find(|key| key.fingerprint == key_fingerprint)
            .ok_or_else(|| {
                IronCrewError::Conflict(format!(
                    "human-input encryption key fingerprint '{key_fingerprint}' is unavailable"
                ))
            })
    }

    fn read_env() -> std::result::Result<Option<Self>, String> {
        let keys = std::env::var_os(HITL_ENCRYPTION_KEYS_ENV);
        let active_key_id = std::env::var_os(HITL_ACTIVE_KEY_ID_ENV);
        match (keys, active_key_id) {
            (None, None) => Ok(None),
            (Some(_), None) => Err(format!(
                "{HITL_ACTIVE_KEY_ID_ENV} is required when {HITL_ENCRYPTION_KEYS_ENV} is set"
            )),
            (None, Some(_)) => Err(format!(
                "{HITL_ENCRYPTION_KEYS_ENV} is required when {HITL_ACTIVE_KEY_ID_ENV} is set"
            )),
            (Some(keys), Some(active_key_id)) => {
                let keys =
                    Zeroizing::new(keys.into_string().map_err(|_| {
                        format!("{HITL_ENCRYPTION_KEYS_ENV} must contain valid UTF-8")
                    })?);
                let active_key_id = active_key_id
                    .into_string()
                    .map_err(|_| format!("{HITL_ACTIVE_KEY_ID_ENV} must contain valid UTF-8"))?;
                Self::parse_json(keys.as_str(), &active_key_id).map(Some)
            }
        }
    }

    fn parse_json(keys_json: &str, active_key_id: &str) -> std::result::Result<Self, String> {
        if keys_json.len() > MAX_KEYRING_JSON_BYTES {
            return Err(format!(
                "{HITL_ENCRYPTION_KEYS_ENV} exceeds {MAX_KEYRING_JSON_BYTES} bytes"
            ));
        }
        validate_key_id(active_key_id)?;

        let UniqueKeyMap(entries) = serde_json::from_str(keys_json).map_err(|_| {
            format!("{HITL_ENCRYPTION_KEYS_ENV} must be a JSON object of base64 keys")
        })?;
        if entries.is_empty() {
            return Err(format!("{HITL_ENCRYPTION_KEYS_ENV} must not be empty"));
        }
        if entries.len() > MAX_KEYS {
            return Err(format!(
                "{HITL_ENCRYPTION_KEYS_ENV} supports at most {MAX_KEYS} keys"
            ));
        }

        let mut keys = Vec::with_capacity(entries.len());
        let mut fingerprints = HashSet::with_capacity(entries.len());
        for (id, encoded) in entries {
            validate_key_id(&id)?;
            let encoded = Zeroizing::new(encoded);
            let decoded =
                Zeroizing::new(BASE64_STANDARD.decode(encoded.as_bytes()).map_err(|_| {
                    format!("{HITL_ENCRYPTION_KEYS_ENV} contains invalid base64 key material")
                })?);
            let canonical = Zeroizing::new(BASE64_STANDARD.encode(&*decoded));
            if decoded.len() != KEY_BYTES || canonical.as_str() != encoded.as_str() {
                return Err(format!(
                    "{HITL_ENCRYPTION_KEYS_ENV} keys must be canonical base64 encodings of 32 bytes"
                ));
            }

            let mut material = [0_u8; KEY_BYTES];
            material.copy_from_slice(&decoded);
            let fingerprint = key_fingerprint(&material);
            if !fingerprints.insert(fingerprint.clone()) {
                material.fill(0);
                return Err(format!(
                    "{HITL_ENCRYPTION_KEYS_ENV} contains duplicate key material"
                ));
            }
            keys.push(HumanInputKey {
                id,
                fingerprint,
                material,
            });
        }

        if !keys.iter().any(|key| key.id == active_key_id) {
            return Err(format!(
                "{HITL_ACTIVE_KEY_ID_ENV} does not identify a configured key"
            ));
        }

        Ok(Self {
            inner: Arc::new(KeyringInner {
                active_key_id: active_key_id.to_owned(),
                keys,
            }),
        })
    }
}

impl fmt::Debug for HumanInputKeyring {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HumanInputKeyring")
            .field("active_key_id", &self.inner.active_key_id)
            .field("active_fingerprint", &self.active_fingerprint())
            .field("key_count", &self.inner.keys.len())
            .finish()
    }
}

struct UniqueKeyMap(Vec<(String, String)>);

impl<'de> Deserialize<'de> for UniqueKeyMap {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_map(UniqueKeyMapVisitor)
    }
}

struct UniqueKeyMapVisitor;

impl<'de> Visitor<'de> for UniqueKeyMapVisitor {
    type Value = UniqueKeyMap;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a JSON object mapping key ids to base64 keys")
    }

    fn visit_map<M>(self, mut map: M) -> std::result::Result<Self::Value, M::Error>
    where
        M: MapAccess<'de>,
    {
        let mut entries = Vec::new();
        let mut ids = HashSet::new();
        while let Some(id) = map.next_key::<String>()? {
            if !ids.insert(id.clone()) {
                return Err(serde::de::Error::custom(
                    "duplicate human-input encryption key id",
                ));
            }
            let encoded = map.next_value::<String>()?;
            entries.push((id, encoded));
            if entries.len() > MAX_KEYS {
                return Err(serde::de::Error::custom(
                    "too many human-input encryption keys",
                ));
            }
        }
        Ok(UniqueKeyMap(entries))
    }
}

fn validate_question(question: &QuestionInfo) -> Result<()> {
    validate_printable("question id", &question.question_id, MAX_QUESTION_ID_BYTES)?;
    if question.prompt.len() > HARD_MAX_PROMPT_BYTES {
        return Err(IronCrewError::Validation(format!(
            "human-input prompt exceeds {HARD_MAX_PROMPT_BYTES} bytes"
        )));
    }
    if question.choices.len() > HARD_MAX_CHOICES {
        return Err(IronCrewError::Validation(format!(
            "human-input choices exceed {HARD_MAX_CHOICES} entries"
        )));
    }
    let choices_bytes = question
        .choices
        .iter()
        .try_fold(0usize, |total, choice| total.checked_add(choice.len()));
    if choices_bytes.is_none_or(|length| length > HARD_MAX_CHOICES_BYTES) {
        return Err(IronCrewError::Validation(format!(
            "human-input choices exceed {HARD_MAX_CHOICES_BYTES} bytes"
        )));
    }
    if !(1..=HARD_MAX_TIMEOUT_SECS).contains(&question.timeout_s) {
        return Err(IronCrewError::Validation(format!(
            "human-input timeout must be between 1 and {HARD_MAX_TIMEOUT_SECS} seconds"
        )));
    }
    if !matches!(question.kind.as_str(), "question" | "approval") {
        return Err(IronCrewError::Validation(
            "human-input kind must be 'question' or 'approval'".into(),
        ));
    }
    if question.asked_at.len() > MAX_ASKED_AT_BYTES
        || chrono::DateTime::parse_from_rfc3339(&question.asked_at).is_err()
    {
        return Err(IronCrewError::Validation(
            "human-input asked_at must be an RFC 3339 timestamp".into(),
        ));
    }
    Ok(())
}

fn validate_printable(label: &str, value: &str, max_bytes: usize) -> Result<()> {
    if value.is_empty() || value.len() > max_bytes || value.chars().any(char::is_control) {
        return Err(IronCrewError::Validation(format!(
            "human-input {label} must be 1-{max_bytes} printable bytes"
        )));
    }
    Ok(())
}

fn validate_fingerprint(label: &str, value: &str) -> Result<()> {
    if value.len() != FINGERPRINT_BYTES
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(IronCrewError::Validation(format!(
            "human-input {label} must be a lowercase SHA-256 digest"
        )));
    }
    Ok(())
}

fn validate_key_id(key_id: &str) -> std::result::Result<(), String> {
    if key_id.is_empty() || key_id.len() > MAX_KEY_ID_BYTES {
        return Err(format!(
            "human-input key ids must be 1-{MAX_KEY_ID_BYTES} bytes"
        ));
    }
    let mut bytes = key_id.bytes();
    let Some(first) = bytes.next() else {
        return Err("human-input key id must not be empty".into());
    };
    if !first.is_ascii_alphanumeric()
        || !bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err(
            "human-input key ids must start with an ASCII letter or digit and contain only ASCII letters, digits, '.', '_', or '-'"
                .into(),
        );
    }
    Ok(())
}

fn key_fingerprint(material: &[u8; KEY_BYTES]) -> String {
    encode_hex(&Sha256::digest(material))
}

fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut fingerprint = String::with_capacity(bytes.len().saturating_mul(2));
    for &byte in bytes {
        fingerprint.push(char::from(HEX[usize::from(byte >> 4)]));
        fingerprint.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    fingerprint
}

fn aead_key(material: &[u8; KEY_BYTES]) -> Result<LessSafeKey> {
    UnboundKey::new(&AES_256_GCM, material)
        .map(LessSafeKey::new)
        .map_err(|_| IronCrewError::Validation("human-input encryption key setup failed".into()))
}

fn authentication_error() -> IronCrewError {
    IronCrewError::Validation("encrypted human-input answer failed authentication".into())
}

struct BoundedCounter {
    bytes: usize,
    max_bytes: usize,
}

impl Write for BoundedCounter {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        self.bytes = self
            .bytes
            .checked_add(buffer.len())
            .ok_or_else(|| std::io::Error::other("human-input answer is too large"))?;
        if self.bytes > self.max_bytes {
            return Err(std::io::Error::other("human-input answer is too large"));
        }
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn configured_max_answer_bytes() -> usize {
    std::env::var("IRONCREW_ASK_HUMAN_MAX_ANSWER_BYTES")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| (1..=HARD_MAX_ANSWER_BYTES).contains(value))
        .unwrap_or(DEFAULT_MAX_ANSWER_BYTES)
}

fn serialize_bounded<T: Serialize>(
    payload: &T,
    max_bytes: usize,
    payload_name: &str,
) -> Result<Vec<u8>> {
    struct BoundedBuffer {
        bytes: Vec<u8>,
        max_bytes: usize,
    }

    impl Write for BoundedBuffer {
        fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
            let new_length = self
                .bytes
                .len()
                .checked_add(buffer.len())
                .ok_or_else(|| std::io::Error::other("human-input answer is too large"))?;
            if new_length > self.max_bytes {
                return Err(std::io::Error::other("human-input answer is too large"));
            }
            self.bytes.extend_from_slice(buffer);
            Ok(buffer.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    let mut output = BoundedBuffer {
        bytes: Vec::with_capacity(256),
        max_bytes,
    };
    serde_json::to_writer(&mut output, payload).map_err(|_| {
        output.bytes.fill(0);
        IronCrewError::Validation(format!(
            "{payload_name} exceeds {max_bytes} serialized bytes"
        ))
    })?;
    Ok(output.bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn encoded_key(byte: u8) -> String {
        BASE64_STANDARD.encode([byte; KEY_BYTES])
    }

    fn keyring(active: &str, keys: &[(&str, u8)]) -> HumanInputKeyring {
        let entries = keys
            .iter()
            .map(|(id, byte)| format!(r#""{id}":"{}""#, encoded_key(*byte)))
            .collect::<Vec<_>>()
            .join(",");
        HumanInputKeyring::from_json(&format!("{{{entries}}}"), active).unwrap()
    }

    fn aad_for(question: &QuestionInfo) -> HumanInputAad {
        HumanInputAad::new(
            "review-flow",
            "run-1",
            "question-1",
            question_digest(question).unwrap(),
            "replica-a",
            "a".repeat(FINGERPRINT_BYTES),
            "attempt-1",
        )
        .unwrap()
    }

    fn aad() -> HumanInputAad {
        aad_for(&question())
    }

    fn question() -> QuestionInfo {
        QuestionInfo {
            question_id: "question-1".into(),
            prompt: "Proceed?".into(),
            choices: vec!["yes".into(), "no".into()],
            asked_at: "2026-07-19T10:00:00Z".into(),
            timeout_s: 600,
            kind: "approval".into(),
        }
    }

    #[test]
    fn json_round_trip_uses_random_nonces() {
        let keyring = keyring("primary", &[("primary", 7)]);
        let answer = json!({"approved": true, "reason": "reviewed"});

        let first = keyring.seal_json(&aad(), &answer).unwrap();
        let second = keyring.seal_json(&aad(), &answer).unwrap();

        assert_ne!(first.nonce, second.nonce);
        assert_ne!(first.ciphertext, second.ciphertext);
        assert_eq!(
            keyring
                .open_json(
                    &aad(),
                    &first.key_fingerprint,
                    &first.nonce,
                    &first.ciphertext,
                )
                .unwrap(),
            answer
        );
    }

    #[test]
    fn question_metadata_round_trip_is_separate_from_answer_limit() {
        let keyring = keyring("primary", &[("primary", 8)]);
        let mut expected = question();
        expected.prompt = "p".repeat(DEFAULT_MAX_ANSWER_BYTES + 1);

        let expected_aad = aad_for(&expected);
        let encrypted = keyring.seal_question(&expected_aad, &expected).unwrap();
        let opened = keyring
            .open_question(
                &expected_aad,
                &encrypted.key_fingerprint,
                &encrypted.nonce,
                &encrypted.ciphertext,
            )
            .unwrap();

        assert_eq!(opened, expected);
        assert!(
            keyring
                .open_json(
                    &aad(),
                    &encrypted.key_fingerprint,
                    &encrypted.nonce,
                    &encrypted.ciphertext,
                )
                .is_err()
        );
    }

    #[test]
    fn question_digest_fences_stable_id_reuse() {
        let original = question();
        let mut revised = original.clone();
        revised.prompt = "Proceed under the revised policy?".into();

        assert_ne!(
            question_digest(&original).unwrap(),
            question_digest(&revised).unwrap()
        );

        let keyring = keyring("primary", &[("primary", 11)]);
        let original_aad = aad_for(&original);
        let revised_aad = aad_for(&revised);
        let encrypted_answer = keyring
            .seal_json(&original_aad, &json!("approved"))
            .unwrap();
        assert!(
            keyring
                .open_json(
                    &revised_aad,
                    &encrypted_answer.key_fingerprint,
                    &encrypted_answer.nonce,
                    &encrypted_answer.ciphertext,
                )
                .is_err(),
            "an answer for old question metadata must not authenticate after id reuse"
        );
        assert!(keyring.seal_question(&revised_aad, &original).is_err());
    }

    #[test]
    fn wrong_key_or_fingerprint_fails_closed() {
        let first_keyring = keyring("first", &[("first", 1)]);
        let second_keyring = keyring("second", &[("second", 2)]);
        let encrypted = first_keyring.seal_json(&aad(), &json!("allow")).unwrap();

        let missing = second_keyring.open_json(
            &aad(),
            &encrypted.key_fingerprint,
            &encrypted.nonce,
            &encrypted.ciphertext,
        );
        assert!(matches!(missing, Err(IronCrewError::Conflict(_))));

        let wrong_key = second_keyring.open_json(
            &aad(),
            second_keyring.active_fingerprint(),
            &encrypted.nonce,
            &encrypted.ciphertext,
        );
        assert!(matches!(wrong_key, Err(IronCrewError::Validation(_))));
    }

    #[test]
    fn tampering_or_aad_rebinding_fails_closed() {
        let keyring = keyring("primary", &[("primary", 9)]);
        let encrypted = keyring.seal_json(&aad(), &json!({"answer": 42})).unwrap();

        let mut tampered = encrypted.ciphertext.clone();
        tampered[0] ^= 1;
        assert!(
            keyring
                .open_json(
                    &aad(),
                    &encrypted.key_fingerprint,
                    &encrypted.nonce,
                    &tampered,
                )
                .is_err()
        );

        let mut rebound = aad();
        rebound.run_id = "run-2".into();
        assert!(
            keyring
                .open_json(
                    &rebound,
                    &encrypted.key_fingerprint,
                    &encrypted.nonce,
                    &encrypted.ciphertext,
                )
                .is_err()
        );

        let mut wrong_nonce = encrypted.nonce.clone();
        wrong_nonce[0] ^= 1;
        assert!(
            keyring
                .open_json(
                    &aad(),
                    &encrypted.key_fingerprint,
                    &wrong_nonce,
                    &encrypted.ciphertext,
                )
                .is_err()
        );
    }

    #[test]
    fn key_rotation_reads_old_and_writes_active() {
        let old = keyring("old", &[("old", 3)]);
        let old_encrypted = old.seal_json(&aad(), &json!("old answer")).unwrap();
        let rotated = keyring("new", &[("old", 3), ("new", 4)]);

        assert_eq!(
            rotated
                .open_json(
                    &aad(),
                    &old_encrypted.key_fingerprint,
                    &old_encrypted.nonce,
                    &old_encrypted.ciphertext,
                )
                .unwrap(),
            json!("old answer")
        );
        let new_encrypted = rotated.seal_json(&aad(), &json!("new answer")).unwrap();
        assert_eq!(new_encrypted.key_fingerprint, rotated.active_fingerprint());
        assert_ne!(new_encrypted.key_fingerprint, old_encrypted.key_fingerprint);

        let pinned_answer = rotated
            .seal_json_for_fingerprint(
                &aad(),
                &json!("answer for an old-key question"),
                &old_encrypted.key_fingerprint,
            )
            .unwrap();
        assert_eq!(pinned_answer.key_fingerprint, old_encrypted.key_fingerprint);
        assert_eq!(
            old.open_json(
                &aad(),
                &pinned_answer.key_fingerprint,
                &pinned_answer.nonce,
                &pinned_answer.ciphertext,
            )
            .unwrap(),
            json!("answer for an old-key question")
        );
        assert!(
            rotated
                .seal_json_for_fingerprint(
                    &aad(),
                    &json!("unavailable"),
                    "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
                )
                .is_err()
        );
    }

    #[test]
    fn configuration_and_payload_bounds_are_enforced() {
        let too_many = (0..=MAX_KEYS)
            .map(|index| format!(r#""key-{index}":"{}""#, encoded_key(index as u8)))
            .collect::<Vec<_>>()
            .join(",");
        assert!(HumanInputKeyring::from_json(&format!("{{{too_many}}}"), "key-0").is_err());
        assert!(
            HumanInputKeyring::from_json(&" ".repeat(MAX_KEYRING_JSON_BYTES + 1), "primary")
                .is_err()
        );
        assert!(HumanInputKeyring::from_json(r#"{"bad id":"AAAAAAAA"}"#, "bad id").is_err());
        assert!(HumanInputKeyring::from_json(r#"{"same":"AQ==","same":"Ag=="}"#, "same").is_err());

        let keyring = keyring("primary", &[("primary", 5)]);
        let oversized = json!("x".repeat(HARD_MAX_ANSWER_BYTES + 1));
        assert!(keyring.seal_json(&aad(), &oversized).is_err());

        let mut invalid_aad = aad();
        invalid_aad.question_id = "q".repeat(MAX_QUESTION_ID_BYTES + 1);
        assert!(keyring.seal_json(&invalid_aad, &json!(true)).is_err());
    }

    #[test]
    fn durable_question_bounds_and_debug_redaction_are_enforced() {
        let registration = DurableHumanInputRegistration {
            flow: "review-flow".into(),
            run_id: "run-1".into(),
            question: question(),
            key_hash: "b".repeat(FINGERPRINT_BYTES),
            attempt_id: "attempt-1".into(),
        };
        registration.validate().unwrap();
        assert!(!format!("{registration:?}").contains("Proceed?"));

        let mut invalid = registration.clone();
        invalid.question.choices = vec!["x".into(); HARD_MAX_CHOICES + 1];
        assert!(invalid.validate().is_err());

        let keyring = keyring("primary", &[("primary", 6)]);
        let key_debug = format!("{keyring:?}");
        assert!(!key_debug.contains(&encoded_key(6)));
        assert_eq!(
            format!("{:?}", HumanInputReadOutcome::Answered(json!("secret"))),
            "Answered(<redacted>)"
        );
    }
}
