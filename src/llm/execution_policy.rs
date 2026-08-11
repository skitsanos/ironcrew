//! Captured, secret-free limits that affect provider request semantics.

use serde_json::{Value, json};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ProviderExecutionPolicy {
    rate_limit_ms: Option<u64>,
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
            "response_bytes": self.response_bytes,
            "error_bytes": self.error_bytes,
            "output_bytes": self.output_bytes,
            "stream_bytes": self.stream_bytes,
        })
    }

    #[cfg(test)]
    pub(crate) fn from_values(
        rate_limit_ms: Option<u64>,
        response_bytes: usize,
        error_bytes: usize,
        output_bytes: usize,
        stream_bytes: usize,
    ) -> Self {
        Self {
            rate_limit_ms,
            response_bytes,
            error_bytes,
            output_bytes,
            stream_bytes,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_effective_provider_limit_changes_the_definition() {
        let base = ProviderExecutionPolicy::from_values(Some(10), 11, 12, 13, 14);
        let expected = base.definition();
        for changed in [
            ProviderExecutionPolicy::from_values(Some(9), 11, 12, 13, 14),
            ProviderExecutionPolicy::from_values(Some(10), 10, 12, 13, 14),
            ProviderExecutionPolicy::from_values(Some(10), 11, 11, 13, 14),
            ProviderExecutionPolicy::from_values(Some(10), 11, 12, 12, 14),
            ProviderExecutionPolicy::from_values(Some(10), 11, 12, 13, 13),
        ] {
            assert_ne!(expected, changed.definition());
        }
    }
}
