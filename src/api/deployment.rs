//! Non-secret deployment evidence exposed by the authenticated capability API.
//!
//! These markers are operator-supplied attestations. Platform acceptance must
//! independently hash the running artifact, flow set, and canonical effective
//! configuration before comparing them with this tuple.

use serde::Serialize;

use crate::utils::error::{IronCrewError, Result};

pub const REVISION_ENV: &str = "IRONCREW_DEPLOYMENT_REVISION";
pub const ARTIFACT_FINGERPRINT_ENV: &str = "IRONCREW_ARTIFACT_FINGERPRINT";
pub const FLOW_FINGERPRINT_ENV: &str = "IRONCREW_FLOW_FINGERPRINT";
pub const CONFIG_FINGERPRINT_ENV: &str = "IRONCREW_CONFIG_FINGERPRINT";
pub const HITL_KEYRING_FINGERPRINT_ENV: &str = "IRONCREW_HITL_KEYRING_FINGERPRINT";

const MAX_REVISION_BYTES: usize = 128;
const SHA256_PREFIX: &str = "sha256:";
const SHA256_HEX_BYTES: usize = 64;

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct DeploymentEvidence {
    pub revision: String,
    pub artifact_fingerprint: String,
    pub flow_fingerprint: String,
    pub config_fingerprint: String,
    pub hitl_keyring_fingerprint: String,
}

#[derive(Clone, Debug)]
pub struct RuntimeIdentity {
    process_start_id: String,
    deployment: Option<DeploymentEvidence>,
}

impl RuntimeIdentity {
    pub fn from_env() -> Result<Self> {
        let read = |name: &str| match std::env::var(name) {
            Ok(value) => Ok(Some(value)),
            Err(std::env::VarError::NotPresent) => Ok(None),
            Err(std::env::VarError::NotUnicode(_)) => Err(IronCrewError::Validation(format!(
                "{name} must contain valid UTF-8"
            ))),
        };
        Self::from_values(
            read(REVISION_ENV)?,
            read(ARTIFACT_FINGERPRINT_ENV)?,
            read(FLOW_FINGERPRINT_ENV)?,
            read(CONFIG_FINGERPRINT_ENV)?,
            read(HITL_KEYRING_FINGERPRINT_ENV)?,
        )
    }

    /// Construct a boot identity without deployment attestations.
    pub fn disabled() -> Self {
        Self::from_validated(None)
    }

    /// Construct a boot identity after validating an optional complete tuple.
    ///
    /// Embedded users that build an [`crate::api::AppState`] directly must go
    /// through this path rather than publishing unchecked evidence.
    pub fn try_new(deployment: Option<DeploymentEvidence>) -> Result<Self> {
        if let Some(evidence) = deployment.as_ref() {
            validate_revision(&evidence.revision)?;
            validate_sha256(ARTIFACT_FINGERPRINT_ENV, &evidence.artifact_fingerprint)?;
            validate_sha256(FLOW_FINGERPRINT_ENV, &evidence.flow_fingerprint)?;
            validate_sha256(CONFIG_FINGERPRINT_ENV, &evidence.config_fingerprint)?;
            validate_sha256(
                HITL_KEYRING_FINGERPRINT_ENV,
                &evidence.hitl_keyring_fingerprint,
            )?;
        }
        Ok(Self::from_validated(deployment))
    }

    fn from_validated(deployment: Option<DeploymentEvidence>) -> Self {
        Self {
            process_start_id: uuid::Uuid::new_v4().to_string(),
            deployment,
        }
    }

    pub fn process_start_id(&self) -> &str {
        &self.process_start_id
    }

    pub fn deployment(&self) -> Option<&DeploymentEvidence> {
        self.deployment.as_ref()
    }

    fn from_values(
        revision: Option<String>,
        artifact_fingerprint: Option<String>,
        flow_fingerprint: Option<String>,
        config_fingerprint: Option<String>,
        hitl_keyring_fingerprint: Option<String>,
    ) -> Result<Self> {
        let configured = [
            revision.is_some(),
            artifact_fingerprint.is_some(),
            flow_fingerprint.is_some(),
            config_fingerprint.is_some(),
            hitl_keyring_fingerprint.is_some(),
        ];
        if !configured.iter().any(|value| *value) {
            return Ok(Self::disabled());
        }
        if !configured.iter().all(|value| *value) {
            return Err(IronCrewError::Validation(format!(
                "{REVISION_ENV}, {ARTIFACT_FINGERPRINT_ENV}, {FLOW_FINGERPRINT_ENV}, {CONFIG_FINGERPRINT_ENV}, and {HITL_KEYRING_FINGERPRINT_ENV} must be configured together"
            )));
        }

        let revision = revision.expect("complete deployment tuple has a revision");
        let artifact_fingerprint =
            artifact_fingerprint.expect("complete deployment tuple has an artifact fingerprint");
        let flow_fingerprint =
            flow_fingerprint.expect("complete deployment tuple has a flow fingerprint");
        let config_fingerprint =
            config_fingerprint.expect("complete deployment tuple has a config fingerprint");
        let hitl_keyring_fingerprint = hitl_keyring_fingerprint
            .expect("complete deployment tuple has a HITL keyring fingerprint");
        Self::try_new(Some(DeploymentEvidence {
            revision,
            artifact_fingerprint,
            flow_fingerprint,
            config_fingerprint,
            hitl_keyring_fingerprint,
        }))
    }
}

fn validate_revision(value: &str) -> Result<()> {
    let valid = !value.is_empty()
        && value.len() <= MAX_REVISION_BYTES
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_' | b':' | b'+')
        });
    if valid {
        Ok(())
    } else {
        Err(IronCrewError::Validation(format!(
            "{REVISION_ENV} must be 1-{MAX_REVISION_BYTES} bytes of ASCII letters, digits, '.', '-', '_', ':', or '+'"
        )))
    }
}

fn validate_sha256(name: &str, value: &str) -> Result<()> {
    let Some(hex) = value.strip_prefix(SHA256_PREFIX) else {
        return Err(IronCrewError::Validation(format!(
            "{name} must be a canonical lowercase sha256 fingerprint"
        )));
    };
    if hex.len() != SHA256_HEX_BYTES
        || !hex
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(IronCrewError::Validation(format!(
            "{name} must be a canonical lowercase sha256 fingerprint"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const HASH: &str = "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn values(mask: u8) -> Result<RuntimeIdentity> {
        RuntimeIdentity::from_values(
            (mask & 1 != 0).then(|| "develop-c4799a3-dirty-deadbeef".to_string()),
            (mask & 2 != 0).then(|| HASH.to_string()),
            (mask & 4 != 0).then(|| HASH.to_string()),
            (mask & 8 != 0).then(|| HASH.to_string()),
            (mask & 16 != 0).then(|| HASH.to_string()),
        )
    }

    #[test]
    fn deployment_tuple_is_all_or_none() {
        assert!(values(0).unwrap().deployment.is_none());
        for mask in 1..31 {
            assert!(values(mask).is_err(), "partial tuple mask {mask} passed");
        }
        let identity = values(31).unwrap();
        assert!(uuid::Uuid::parse_str(identity.process_start_id()).is_ok());
        assert_eq!(identity.deployment().unwrap().artifact_fingerprint, HASH);
    }

    #[test]
    fn deployment_tuple_rejects_noncanonical_values_without_echoing_them() {
        let bad_revision = RuntimeIdentity::from_values(
            Some("bad revision".into()),
            Some(HASH.into()),
            Some(HASH.into()),
            Some(HASH.into()),
            Some(HASH.into()),
        )
        .unwrap_err()
        .to_string();
        assert!(bad_revision.contains(REVISION_ENV));
        assert!(!bad_revision.contains("bad revision"));

        for bad_hash in [
            "0123456789abcdef",
            "sha256:ABCDEF0123456789abcdef0123456789abcdef0123456789abcdef0123456789",
            "sha256:gggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggg",
        ] {
            let error = RuntimeIdentity::from_values(
                Some("revision-1".into()),
                Some(bad_hash.into()),
                Some(HASH.into()),
                Some(HASH.into()),
                Some(HASH.into()),
            )
            .unwrap_err()
            .to_string();
            assert!(error.contains(ARTIFACT_FINGERPRINT_ENV));
            assert!(!error.contains(bad_hash));
        }

        let unchecked_public_path = RuntimeIdentity::try_new(Some(DeploymentEvidence {
            revision: "revision-1".into(),
            artifact_fingerprint: "not-a-fingerprint".into(),
            flow_fingerprint: HASH.into(),
            config_fingerprint: HASH.into(),
            hitl_keyring_fingerprint: HASH.into(),
        }))
        .unwrap_err()
        .to_string();
        assert!(unchecked_public_path.contains(ARTIFACT_FINGERPRINT_ENV));
        assert!(!unchecked_public_path.contains("not-a-fingerprint"));
    }
}
