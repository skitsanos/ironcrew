//! Captured, secret-free limits that affect provider request semantics.

use serde_json::{Value, json};

use crate::utils::error::{IronCrewError, Result};

pub(crate) const DEFAULT_PROVIDER_REQUEST_BYTES: usize = 32 * 1024 * 1024;
pub(crate) const HARD_PROVIDER_REQUEST_BYTES: usize = 256 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ProviderExecutionPolicy {
    rate_limit_ms: Option<u64>,
    request_bytes: usize,
    response_bytes: usize,
    error_bytes: usize,
    output_bytes: usize,
    stream_bytes: usize,
}

impl ProviderExecutionPolicy {
    pub(crate) fn capture() -> Self {
        Self {
            rate_limit_ms: std::env::var("IRONCREW_RATE_LIMIT_MS")
                .ok()
                .and_then(|value| value.parse::<u64>().ok())
                .filter(|value| *value > 0),
            request_bytes: request_bytes_from_raw(
                std::env::var("IRONCREW_PROVIDER_MAX_REQUEST_BYTES")
                    .ok()
                    .as_deref(),
            ),
            response_bytes: crate::utils::http::byte_limit_from_env(
                "IRONCREW_PROVIDER_MAX_RESPONSE_BYTES",
                crate::utils::http::DEFAULT_PROVIDER_RESPONSE_BYTES,
            ),
            error_bytes: crate::utils::http::byte_limit_from_env(
                "IRONCREW_PROVIDER_MAX_ERROR_BYTES",
                crate::utils::http::DEFAULT_PROVIDER_ERROR_BYTES,
            ),
            output_bytes: crate::utils::http::byte_limit_from_env(
                "IRONCREW_PROVIDER_MAX_OUTPUT_BYTES",
                crate::utils::http::DEFAULT_PROVIDER_OUTPUT_BYTES,
            ),
            stream_bytes: crate::utils::http::byte_limit_from_env(
                "IRONCREW_PROVIDER_MAX_STREAM_BYTES",
                crate::utils::http::DEFAULT_PROVIDER_STREAM_BYTES,
            ),
        }
    }

    pub(crate) fn rate_limit_ms(self) -> Option<u64> {
        self.rate_limit_ms
    }

    pub(crate) fn request_bytes(self) -> usize {
        self.request_bytes
    }

    /// Serialize exactly the bytes that will be sent on the wire and reject
    /// oversized provider requests before constructing or sending an HTTP
    /// request. The error reports lengths only, never request content.
    pub(crate) fn serialize_request(self, provider: &'static str, body: &Value) -> Result<Vec<u8>> {
        let serialized = serde_json::to_vec(body).map_err(|_| {
            IronCrewError::Provider(format!("failed to serialize {provider} request body"))
        })?;
        if serialized.len() > self.request_bytes() {
            return Err(IronCrewError::ProviderRequestTooLarge {
                provider,
                actual: serialized.len(),
                limit: self.request_bytes(),
            });
        }
        Ok(serialized)
    }

    pub(crate) fn response_bytes(self) -> usize {
        self.response_bytes
    }

    pub(crate) fn error_bytes(self) -> usize {
        self.error_bytes
    }

    pub(crate) fn output_bytes(self) -> usize {
        self.output_bytes
    }

    pub(crate) fn stream_bytes(self) -> usize {
        self.stream_bytes
    }

    pub(crate) fn definition(self) -> Value {
        json!({
            "rate_limit_ms": self.rate_limit_ms,
            "request_bytes": self.request_bytes,
            "response_bytes": self.response_bytes,
            "error_bytes": self.error_bytes,
            "output_bytes": self.output_bytes,
            "stream_bytes": self.stream_bytes,
        })
    }

    #[cfg(test)]
    pub(crate) fn from_values(
        rate_limit_ms: Option<u64>,
        request_bytes: usize,
        response_bytes: usize,
        error_bytes: usize,
        output_bytes: usize,
        stream_bytes: usize,
    ) -> Self {
        Self {
            rate_limit_ms,
            request_bytes,
            response_bytes,
            error_bytes,
            output_bytes,
            stream_bytes,
        }
    }
}

fn request_bytes_from_raw(raw: Option<&str>) -> usize {
    raw.and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .map(|value| value.min(HARD_PROVIDER_REQUEST_BYTES))
        .unwrap_or(DEFAULT_PROVIDER_REQUEST_BYTES)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_effective_provider_limit_changes_the_definition() {
        let base = ProviderExecutionPolicy::from_values(Some(10), 11, 12, 13, 14, 15);
        let expected = base.definition();
        for changed in [
            ProviderExecutionPolicy::from_values(Some(9), 11, 12, 13, 14, 15),
            ProviderExecutionPolicy::from_values(Some(10), 10, 12, 13, 14, 15),
            ProviderExecutionPolicy::from_values(Some(10), 11, 11, 13, 14, 15),
            ProviderExecutionPolicy::from_values(Some(10), 11, 12, 12, 14, 15),
            ProviderExecutionPolicy::from_values(Some(10), 11, 12, 13, 13, 15),
            ProviderExecutionPolicy::from_values(Some(10), 11, 12, 13, 14, 14),
        ] {
            assert_ne!(expected, changed.definition());
        }
    }

    #[test]
    fn request_limit_has_validated_default_and_hard_ceiling() {
        assert_eq!(request_bytes_from_raw(None), DEFAULT_PROVIDER_REQUEST_BYTES);
        assert_eq!(
            request_bytes_from_raw(Some("0")),
            DEFAULT_PROVIDER_REQUEST_BYTES
        );
        assert_eq!(
            request_bytes_from_raw(Some("not-a-number")),
            DEFAULT_PROVIDER_REQUEST_BYTES
        );
        assert_eq!(request_bytes_from_raw(Some("18000")), 18_000);
        assert_eq!(
            request_bytes_from_raw(Some(&(HARD_PROVIDER_REQUEST_BYTES + 1).to_string())),
            HARD_PROVIDER_REQUEST_BYTES
        );
    }

    #[test]
    fn serialized_request_accepts_exact_boundary_and_rejects_one_byte_less() {
        let body = json!({"message": "no secrets are included in errors"});
        let actual = serde_json::to_vec(&body).unwrap().len();
        let at_boundary = ProviderExecutionPolicy::from_values(None, actual, 2, 3, 4, 5);
        assert_eq!(
            at_boundary
                .serialize_request("OpenAI", &body)
                .unwrap()
                .len(),
            actual
        );

        let below_boundary = ProviderExecutionPolicy::from_values(None, actual - 1, 2, 3, 4, 5);
        let error = below_boundary
            .serialize_request("OpenAI", &body)
            .unwrap_err();
        assert!(matches!(
            error,
            IronCrewError::ProviderRequestTooLarge {
                provider: "OpenAI",
                actual: observed,
                limit
            } if observed == actual && limit == actual - 1
        ));
        assert!(!error.to_string().contains("no secrets"));
    }
}
