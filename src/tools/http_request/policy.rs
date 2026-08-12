use serde_json::{Value, json};

use super::{
    DEFAULT_REQUEST_BODY_BYTES, DEFAULT_REQUEST_HEADER_BYTES, HARD_REQUEST_BODY_BYTES,
    HARD_REQUEST_HEADER_BYTES,
};

#[derive(Clone, Debug)]
pub(crate) struct HttpToolPolicy {
    request_header_bytes: usize,
    request_body_bytes: usize,
    response_header_bytes: usize,
    response_bytes: usize,
    json_bytes: usize,
    output_bytes: usize,
    allow_private: bool,
}

impl HttpToolPolicy {
    pub(crate) fn capture() -> Self {
        Self {
            request_header_bytes: crate::utils::http::byte_limit_from_env(
                "IRONCREW_HTTP_MAX_REQUEST_HEADER_BYTES",
                DEFAULT_REQUEST_HEADER_BYTES,
            )
            .min(HARD_REQUEST_HEADER_BYTES),
            request_body_bytes: crate::utils::http::byte_limit_from_env(
                "IRONCREW_HTTP_MAX_REQUEST_BODY_BYTES",
                DEFAULT_REQUEST_BODY_BYTES,
            )
            .min(HARD_REQUEST_BODY_BYTES),
            response_header_bytes: crate::utils::http::byte_limit_from_env(
                "IRONCREW_HTTP_MAX_HEADER_BYTES",
                crate::utils::http::DEFAULT_HTTP_HEADER_BYTES,
            ),
            response_bytes: crate::utils::http::byte_limit_from_env_with_legacy(
                "IRONCREW_HTTP_MAX_RESPONSE_BYTES",
                "IRONCREW_MAX_RESPONSE_SIZE",
                crate::utils::http::DEFAULT_HTTP_TOOL_RESPONSE_BYTES,
            ),
            json_bytes: crate::utils::http::byte_limit_from_env(
                "IRONCREW_HTTP_MAX_JSON_BYTES",
                crate::utils::http::DEFAULT_HTTP_JSON_PARSE_BYTES,
            ),
            output_bytes: crate::utils::http::byte_limit_from_env(
                "IRONCREW_HTTP_MAX_OUTPUT_BYTES",
                crate::utils::http::DEFAULT_HTTP_TOOL_OUTPUT_BYTES,
            ),
            allow_private: crate::utils::network::private_ips_override_enabled(),
        }
    }

    #[cfg(test)]
    pub(crate) fn from_values(marker: usize, allow_private: bool) -> Self {
        Self {
            request_header_bytes: marker,
            request_body_bytes: marker,
            response_header_bytes: marker,
            response_bytes: marker,
            json_bytes: marker,
            output_bytes: marker,
            allow_private,
        }
    }

    pub(crate) fn request_header_bytes(&self) -> usize {
        self.request_header_bytes
    }

    pub(crate) fn request_body_bytes(&self) -> usize {
        self.request_body_bytes
    }

    pub(crate) fn response_header_bytes(&self) -> usize {
        self.response_header_bytes
    }

    pub(crate) fn response_bytes(&self) -> usize {
        self.response_bytes
    }

    pub(crate) fn json_bytes(&self) -> usize {
        self.json_bytes
    }

    pub(crate) fn output_bytes(&self) -> usize {
        self.output_bytes
    }

    pub(crate) fn allow_private(&self) -> bool {
        self.allow_private
    }

    pub(crate) fn definition(&self) -> Value {
        json!({
            "request_header_bytes": self.request_header_bytes,
            "request_body_bytes": self.request_body_bytes,
            "response_header_bytes": self.response_header_bytes,
            "response_bytes": self.response_bytes,
            "json_bytes": self.json_bytes,
            "output_bytes": self.output_bytes,
            "allow_private": self.allow_private,
        })
    }

    pub(crate) fn lua_definition(&self) -> Value {
        json!({
            "request_header_bytes": self.request_header_bytes,
            "request_body_bytes": self.request_body_bytes,
            "response_header_bytes": self.response_header_bytes,
            "response_bytes": self.response_bytes,
            "json_bytes": self.json_bytes,
            "allow_private": self.allow_private,
        })
    }
}
