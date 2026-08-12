use std::collections::HashSet;

use axum::{
    extract::{Request, State},
    http::{HeaderMap, HeaderValue, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};

use crate::engine::idempotency::PrincipalId;
use crate::utils::error::{IronCrewError, Result};

const MAX_AUTH_TOKENS: usize = 256;
const MIN_TOKEN_BYTES: usize = 32;
const MAX_TOKEN_BYTES: usize = 4096;
const MAX_TOKEN_MAP_BYTES: usize = 2 * 1024 * 1024;
const MAX_PRINCIPAL_BYTES: usize = 128;
const DEFAULT_LEGACY_PRINCIPAL: &str = "default";

/// Trusted request identity issued only after authentication succeeds.
///
/// `name` is an operator-controlled label suitable for audit records. `id` is
/// the stable, opaque digest used by admission and durable quota accounting;
/// it is never derived from the bearer token or a caller-supplied header.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Principal {
    id: PrincipalId,
    name: String,
    authenticated: bool,
}

impl Principal {
    fn authenticated(name: String, id: PrincipalId) -> Self {
        Self {
            id,
            name,
            authenticated: true,
        }
    }

    fn anonymous() -> Self {
        Self {
            id: PrincipalId::anonymous(),
            name: "anonymous".into(),
            authenticated: false,
        }
    }

    pub fn id(&self) -> &PrincipalId {
        &self.id
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn is_authenticated(&self) -> bool {
        self.authenticated
    }
}

struct Credential {
    principal: Principal,
    token_digest: [u8; 32],
}

/// Preserve object entries long enough to reject duplicate principal labels.
/// Deserializing directly into `serde_json::Map` would silently keep only the
/// last value for a duplicate key, which is too ambiguous for credential
/// configuration.
struct UniqueTokenMap(Vec<(String, serde_json::Value)>);

impl<'de> serde::Deserialize<'de> for UniqueTokenMap {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct UniqueTokenMapVisitor;

        impl<'de> serde::de::Visitor<'de> for UniqueTokenMapVisitor {
            type Value = UniqueTokenMap;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a JSON object with unique API principal labels")
            }

            fn visit_map<M>(self, mut map: M) -> std::result::Result<Self::Value, M::Error>
            where
                M: serde::de::MapAccess<'de>,
            {
                let mut entries = Vec::with_capacity(map.size_hint().unwrap_or(0));
                let mut principals = HashSet::with_capacity(map.size_hint().unwrap_or(0));
                while let Some(name) = map.next_key::<String>()? {
                    if !principals.insert(name.clone()) {
                        return Err(<M::Error as serde::de::Error>::custom(format!(
                            "duplicate API principal '{name}'"
                        )));
                    }
                    entries.push((name, map.next_value::<serde_json::Value>()?));
                }
                Ok(UniqueTokenMap(entries))
            }
        }

        deserializer.deserialize_map(UniqueTokenMapVisitor)
    }
}

/// Immutable authentication policy parsed once at server startup.
///
/// `IRONCREW_API_TOKEN` remains the backwards-compatible single-token input.
/// Its audit/quota label comes from `IRONCREW_API_PRINCIPAL` (default
/// `default`). `IRONCREW_API_TOKENS` may additionally contain a JSON object
/// whose keys are principal labels and whose string values are bearer tokens.
pub struct AuthConfig {
    credentials: Vec<Credential>,
}

impl AuthConfig {
    pub fn from_env() -> Result<Self> {
        let legacy_token = read_optional_env("IRONCREW_API_TOKEN")?;
        let legacy_principal = read_optional_env("IRONCREW_API_PRINCIPAL")?;
        let token_map = read_optional_env("IRONCREW_API_TOKENS")?;
        Self::from_sources(
            legacy_token.as_deref(),
            legacy_principal.as_deref(),
            token_map.as_deref(),
        )
    }

    pub fn disabled() -> Self {
        Self {
            credentials: Vec::new(),
        }
    }

    pub fn is_configured(&self) -> bool {
        !self.credentials.is_empty()
    }

    pub fn principal_count(&self) -> usize {
        self.credentials.len()
    }

    fn from_sources(
        legacy_token: Option<&str>,
        legacy_principal: Option<&str>,
        token_map: Option<&str>,
    ) -> Result<Self> {
        if legacy_principal.is_some() && legacy_token.is_none() {
            return Err(IronCrewError::Validation(
                "IRONCREW_API_PRINCIPAL requires IRONCREW_API_TOKEN".into(),
            ));
        }

        let mut credentials = Vec::new();
        if let Some(token) = legacy_token {
            validate_token("IRONCREW_API_TOKEN", token)?;
            let (name, id) = match legacy_principal {
                Some(name) => {
                    validate_principal(name)?;
                    (name.to_string(), PrincipalId::from_label(name))
                }
                None => (DEFAULT_LEGACY_PRINCIPAL.to_string(), PrincipalId::legacy()),
            };
            credentials.push(Credential {
                principal: Principal::authenticated(name, id),
                token_digest: token_digest(token),
            });
        }

        if let Some(raw) = token_map {
            if raw.len() > MAX_TOKEN_MAP_BYTES {
                return Err(IronCrewError::Validation(format!(
                    "IRONCREW_API_TOKENS exceeds the {MAX_TOKEN_MAP_BYTES}-byte configuration limit"
                )));
            }
            let parsed = serde_json::from_str::<UniqueTokenMap>(raw).map_err(|error| {
                    IronCrewError::Validation(format!(
                        "IRONCREW_API_TOKENS must be a JSON object of principal-to-token strings: {error}"
                    ))
                })?;
            if parsed.0.is_empty() {
                return Err(IronCrewError::Validation(
                    "IRONCREW_API_TOKENS must contain at least one principal".into(),
                ));
            }
            for (name, value) in parsed.0 {
                validate_principal(&name)?;
                let token = value.as_str().ok_or_else(|| {
                    IronCrewError::Validation(format!(
                        "IRONCREW_API_TOKENS entry '{name}' must be a string"
                    ))
                })?;
                validate_token("IRONCREW_API_TOKENS bearer token", token)?;
                credentials.push(Credential {
                    principal: Principal::authenticated(
                        name.clone(),
                        PrincipalId::from_label(&name),
                    ),
                    token_digest: token_digest(token),
                });
            }
        }

        if credentials.len() > MAX_AUTH_TOKENS {
            return Err(IronCrewError::Validation(format!(
                "At most {MAX_AUTH_TOKENS} API bearer tokens may be configured"
            )));
        }

        let mut principals = HashSet::with_capacity(credentials.len());
        let mut tokens = HashSet::with_capacity(credentials.len());
        for credential in &credentials {
            if !principals.insert(credential.principal.name.clone()) {
                return Err(IronCrewError::Validation(format!(
                    "Duplicate API principal '{}'",
                    credential.principal.name
                )));
            }
            if !tokens.insert(credential.token_digest) {
                return Err(IronCrewError::Validation(
                    "The same API bearer token cannot be assigned to multiple principals".into(),
                ));
            }
        }

        Ok(Self { credentials })
    }

    fn authenticate(&self, headers: &HeaderMap) -> std::result::Result<Principal, AuthFailure> {
        if !self.is_configured() {
            return Ok(Principal::anonymous());
        }

        let header_value = headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .ok_or(AuthFailure::Missing)?;
        let (scheme, token) = header_value
            .split_once(' ')
            .ok_or(AuthFailure::InvalidScheme)?;
        if !scheme.eq_ignore_ascii_case("Bearer") || token.is_empty() {
            return Err(AuthFailure::InvalidScheme);
        }
        if !valid_token_bytes(token) {
            // Apply the same bound to untrusted request input before hashing;
            // configured credentials can only use this HTTP-representable
            // alphabet as well.
            return Err(AuthFailure::InvalidToken);
        }

        let supplied_digest = token_digest(token);
        // Scan every configured digest so the match position does not become a
        // useful timing signal. Duplicate token digests are rejected at boot.
        let mut matched = None;
        for credential in &self.credentials {
            if constant_time_digest_eq(&supplied_digest, &credential.token_digest) {
                matched = Some(credential.principal.clone());
            }
        }
        matched.ok_or(AuthFailure::InvalidToken)
    }
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self::disabled()
    }
}

#[derive(Clone, Copy, Debug)]
enum AuthFailure {
    Missing,
    InvalidScheme,
    InvalidToken,
}

impl AuthFailure {
    fn message(self) -> &'static str {
        match self {
            Self::Missing => "Missing Authorization header",
            Self::InvalidScheme => "Invalid Authorization scheme",
            Self::InvalidToken => "Invalid token",
        }
    }
}

/// Authenticate protected routes, inject a trusted principal, and replace a
/// caller-supplied audit label only when authentication was actually required.
pub async fn bearer_auth(
    State(config): State<std::sync::Arc<AuthConfig>>,
    mut request: Request,
    next: Next,
) -> Response {
    let principal = match config.authenticate(request.headers()) {
        Ok(principal) => principal,
        Err(error) => {
            return (
                StatusCode::UNAUTHORIZED,
                [(axum::http::header::CACHE_CONTROL, "no-store")],
                axum::Json(serde_json::json!({"error": error.message()})),
            )
                .into_response();
        }
    };

    if principal.is_authenticated() {
        // Principal validation guarantees visible ASCII, so conversion cannot
        // fail. Overwriting prevents X-Audit-Actor from becoming an identity
        // spoofing or quota-splitting primitive on authenticated deployments.
        let value = HeaderValue::from_str(principal.name())
            .expect("validated API principal must be a valid header value");
        request.headers_mut().insert("x-audit-actor", value);
    }
    request.extensions_mut().insert(principal);
    next.run(request).await
}

fn read_optional_env(name: &str) -> Result<Option<String>> {
    match std::env::var(name) {
        Ok(value) => Ok(Some(value)),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => Err(IronCrewError::Validation(format!(
            "{name} must contain valid UTF-8"
        ))),
    }
}

fn validate_token(label: &str, token: &str) -> Result<()> {
    if !valid_token_bytes(token) {
        return Err(IronCrewError::Validation(format!(
            "{label} must be {MIN_TOKEN_BYTES}-{MAX_TOKEN_BYTES} visible ASCII bytes without spaces"
        )));
    }
    Ok(())
}

fn valid_token_bytes(token: &str) -> bool {
    (MIN_TOKEN_BYTES..=MAX_TOKEN_BYTES).contains(&token.len())
        && token.bytes().all(|byte| (b'!'..=b'~').contains(&byte))
}

fn validate_principal(principal: &str) -> Result<()> {
    let mut bytes = principal.bytes();
    let valid_first = bytes
        .next()
        .is_some_and(|byte| byte.is_ascii_alphanumeric());
    let valid_rest = bytes.all(|byte| {
        byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b':' | b'@' | b'-')
    });
    if principal.len() > MAX_PRINCIPAL_BYTES || !valid_first || !valid_rest {
        return Err(IronCrewError::Validation(format!(
            "API principal must be 1-{MAX_PRINCIPAL_BYTES} ASCII bytes, start with an alphanumeric character, and contain only alphanumerics or ._:@-"
        )));
    }
    Ok(())
}

fn token_digest(token: &str) -> [u8; 32] {
    let digest = ring::digest::digest(&ring::digest::SHA256, token.as_bytes());
    let mut output = [0_u8; 32];
    output.copy_from_slice(digest.as_ref());
    output
}

fn constant_time_digest_eq(supplied: &[u8; 32], expected: &[u8; 32]) -> bool {
    let comparison_key = ring::hmac::Key::new(
        ring::hmac::HMAC_SHA256,
        b"ironcrew-api-token-constant-time-comparison",
    );
    let expected_tag = ring::hmac::sign(&comparison_key, expected);
    ring::hmac::verify(&comparison_key, supplied, expected_tag.as_ref()).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::Extension;
    use axum::routing::post;
    use axum::{Json, Router};

    const TOKEN_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const TOKEN_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    fn bearer(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {value}")).unwrap(),
        );
        headers
    }

    #[test]
    fn legacy_token_uses_stable_legacy_principal() {
        let config = AuthConfig::from_sources(Some(TOKEN_A), None, None).unwrap();
        let principal = config.authenticate(&bearer(TOKEN_A)).unwrap();
        assert_eq!(principal.name(), DEFAULT_LEGACY_PRINCIPAL);
        assert_eq!(principal.id(), &PrincipalId::legacy());
        assert!(principal.is_authenticated());
    }

    #[test]
    fn named_legacy_and_json_tokens_resolve_distinct_principals() {
        let map = format!(r#"{{"worker-b":"{TOKEN_B}"}}"#);
        let config = AuthConfig::from_sources(Some(TOKEN_A), Some("worker-a"), Some(&map)).unwrap();
        let first = config.authenticate(&bearer(TOKEN_A)).unwrap();
        let second = config.authenticate(&bearer(TOKEN_B)).unwrap();
        assert_eq!(first.name(), "worker-a");
        assert_eq!(second.name(), "worker-b");
        assert_ne!(first.id(), second.id());
        assert_eq!(config.principal_count(), 2);
    }

    #[test]
    fn duplicate_tokens_and_principals_fail_closed() {
        let duplicate_token = format!(r#"{{"worker-b":"{TOKEN_A}"}}"#);
        assert!(
            AuthConfig::from_sources(Some(TOKEN_A), Some("worker-a"), Some(&duplicate_token))
                .is_err()
        );

        let duplicate_principal = format!(r#"{{"worker-a":"{TOKEN_B}"}}"#);
        assert!(
            AuthConfig::from_sources(Some(TOKEN_A), Some("worker-a"), Some(&duplicate_principal))
                .is_err()
        );

        let duplicate_json_key = format!(r#"{{"worker":"{TOKEN_A}","worker":"{TOKEN_B}"}}"#);
        assert!(AuthConfig::from_sources(None, None, Some(&duplicate_json_key)).is_err());
    }

    #[test]
    fn invalid_token_map_inputs_fail_closed() {
        assert!(AuthConfig::from_sources(None, Some("orphan"), None).is_err());
        assert!(AuthConfig::from_sources(None, None, Some("{}")).is_err());
        assert!(AuthConfig::from_sources(None, None, Some(r#"{"bad name":"value"}"#)).is_err());
        assert!(AuthConfig::from_sources(None, None, Some(r#"{"valid":42}"#)).is_err());

        let entries = (0..=MAX_AUTH_TOKENS)
            .map(|index| format!(r#""p{index}":"token-{index:04}-aaaaaaaaaaaaaaaaaaaaaaaa""#))
            .collect::<Vec<_>>()
            .join(",");
        assert!(AuthConfig::from_sources(None, None, Some(&format!("{{{entries}}}"))).is_err());

        let unicode_token = "é".repeat(MIN_TOKEN_BYTES / 2);
        assert_eq!(unicode_token.len(), MIN_TOKEN_BYTES);
        assert!(AuthConfig::from_sources(Some(&unicode_token), None, None).is_err());
    }

    #[test]
    fn anonymous_mode_preserves_anonymous_identity() {
        let config = AuthConfig::disabled();
        let principal = config.authenticate(&HeaderMap::new()).unwrap();
        assert_eq!(principal.id(), &PrincipalId::anonymous());
        assert!(!principal.is_authenticated());
    }

    #[test]
    fn malformed_or_unknown_authorization_is_rejected() {
        let config = AuthConfig::from_sources(Some(TOKEN_A), None, None).unwrap();
        assert!(matches!(
            config.authenticate(&HeaderMap::new()),
            Err(AuthFailure::Missing)
        ));
        let mut malformed = HeaderMap::new();
        malformed.insert(
            axum::http::header::AUTHORIZATION,
            HeaderValue::from_static("Basic abc"),
        );
        assert!(matches!(
            config.authenticate(&malformed),
            Err(AuthFailure::InvalidScheme)
        ));
        assert!(matches!(
            config.authenticate(&bearer(TOKEN_B)),
            Err(AuthFailure::InvalidToken)
        ));
        assert!(matches!(
            config.authenticate(&bearer(&"a".repeat(MAX_TOKEN_BYTES + 1))),
            Err(AuthFailure::InvalidToken)
        ));
    }

    #[tokio::test]
    async fn authentication_precedes_admission_and_overwrites_audit_actor() {
        let auth = std::sync::Arc::new(
            AuthConfig::from_sources(Some(TOKEN_A), Some("trusted-worker"), None).unwrap(),
        );
        let admission = std::sync::Arc::new(crate::api::admission::AdmissionController::new(
            crate::api::admission::AdmissionConfig {
                work: crate::api::admission::RatePolicy {
                    rate_per_minute: 60,
                    burst: 1,
                },
                control: crate::api::admission::RatePolicy {
                    rate_per_minute: 60,
                    burst: 1,
                },
            },
        ));
        async fn echo(
            Extension(principal): Extension<Principal>,
            headers: HeaderMap,
        ) -> Json<serde_json::Value> {
            Json(serde_json::json!({
                "principal": principal.name(),
                "actor": headers
                    .get("x-audit-actor")
                    .and_then(|value| value.to_str().ok()),
            }))
        }
        let app = Router::new()
            .route("/work", post(echo))
            .layer(axum::middleware::from_fn_with_state(
                admission,
                crate::api::admission::enforce_mutation_admission,
            ))
            .layer(axum::middleware::from_fn_with_state(auth, bearer_auth));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = reqwest::Client::new();

        let first = client
            .post(format!("http://{address}/work"))
            .bearer_auth(TOKEN_A)
            .header("X-Audit-Actor", "spoofed")
            .send()
            .await
            .unwrap();
        assert_eq!(first.status(), StatusCode::OK);
        let body: serde_json::Value = first.json().await.unwrap();
        assert_eq!(body["principal"], "trusted-worker");
        assert_eq!(body["actor"], "trusted-worker");

        let limited = client
            .post(format!("http://{address}/work"))
            .bearer_auth(TOKEN_A)
            .send()
            .await
            .unwrap();
        assert_eq!(limited.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(
            limited.headers()[axum::http::header::CACHE_CONTROL],
            "no-store"
        );
        assert!(
            limited
                .headers()
                .contains_key(axum::http::header::RETRY_AFTER)
        );
        server.abort();
    }
}
