use std::time::Duration;

use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Request, State},
    http::{HeaderValue, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};

use crate::api::ErrorResponse;
use crate::utils::error::{IronCrewError, Result};

const DEFAULT_HEADER_READ_TIMEOUT_SECS: u64 = 10;
const MAX_HEADER_READ_TIMEOUT_SECS: u64 = 300;
const DEFAULT_REQUEST_TIMEOUT_SECS: u64 = 600;
const MAX_REQUEST_TIMEOUT_SECS: u64 = 7_200;
const DEFAULT_MAX_CONNECTIONS: u64 = 1_024;
const MAX_CONNECTIONS: u64 = 100_000;
const DEFAULT_MAX_BODY_SIZE: u64 = 10 * 1024 * 1024;
const MAX_BODY_SIZE: u64 = 64 * 1024 * 1024;

#[derive(Clone, Copy, Debug)]
pub(super) struct HttpLimits {
    pub header_read_timeout: Duration,
    pub request_timeout: Duration,
    pub max_connections: usize,
    max_body_size: usize,
}

impl HttpLimits {
    pub fn from_env() -> Result<Self> {
        Ok(Self {
            header_read_timeout: Duration::from_secs(env_u64(
                "IRONCREW_HTTP_HEADER_TIMEOUT_SECS",
                DEFAULT_HEADER_READ_TIMEOUT_SECS,
                1,
                MAX_HEADER_READ_TIMEOUT_SECS,
            )?),
            request_timeout: Duration::from_secs(env_u64(
                "IRONCREW_HTTP_REQUEST_TIMEOUT_SECS",
                DEFAULT_REQUEST_TIMEOUT_SECS,
                1,
                MAX_REQUEST_TIMEOUT_SECS,
            )?),
            max_connections: env_u64(
                "IRONCREW_MAX_HTTP_CONNECTIONS",
                DEFAULT_MAX_CONNECTIONS,
                1,
                MAX_CONNECTIONS,
            )? as usize,
            max_body_size: env_u64(
                "IRONCREW_MAX_BODY_SIZE",
                DEFAULT_MAX_BODY_SIZE,
                1,
                MAX_BODY_SIZE,
            )? as usize,
        })
    }

    pub fn apply(self, app: Router) -> Router {
        app.layer(DefaultBodyLimit::max(self.max_body_size)).layer(
            axum::middleware::from_fn_with_state(self.request_timeout, enforce_request_timeout),
        )
    }

    #[cfg(test)]
    pub fn for_test(
        header_read_timeout: Duration,
        request_timeout: Duration,
        max_connections: usize,
    ) -> Self {
        Self {
            header_read_timeout,
            request_timeout,
            max_connections,
            max_body_size: DEFAULT_MAX_BODY_SIZE as usize,
        }
    }
}

fn env_u64(name: &str, default: u64, min: u64, max: u64) -> Result<u64> {
    let value = match std::env::var(name) {
        Ok(raw) => parse_u64(name, Some(&raw), default, min, max)?,
        Err(std::env::VarError::NotPresent) => default,
        Err(std::env::VarError::NotUnicode(_)) => {
            return Err(IronCrewError::Validation(format!(
                "{name} must contain valid UTF-8"
            )));
        }
    };
    Ok(value)
}

fn parse_u64(name: &str, raw: Option<&str>, default: u64, min: u64, max: u64) -> Result<u64> {
    let value = match raw {
        Some(raw) => raw.parse::<u64>().map_err(|_| {
            IronCrewError::Validation(format!("{name} must be an integer between {min} and {max}"))
        })?,
        None => default,
    };
    if !(min..=max).contains(&value) {
        return Err(IronCrewError::Validation(format!(
            "{name} must be between {min} and {max}"
        )));
    }
    Ok(value)
}

pub(super) async fn enforce_request_timeout(
    State(timeout): State<Duration>,
    request: Request,
    next: Next,
) -> Response {
    match tokio::time::timeout(timeout, next.run(request)).await {
        Ok(response) => response,
        Err(_) => (
            StatusCode::REQUEST_TIMEOUT,
            [(header::CACHE_CONTROL, HeaderValue::from_static("no-store"))],
            Json(ErrorResponse {
                error: format!(
                    "HTTP request exceeded IRONCREW_HTTP_REQUEST_TIMEOUT_SECS ({})",
                    timeout.as_secs()
                ),
            }),
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::parse_u64;

    #[test]
    fn transport_limits_are_strict_and_bounded() {
        assert_eq!(parse_u64("LIMIT", None, 10, 1, 20).unwrap(), 10);
        assert_eq!(parse_u64("LIMIT", Some("20"), 10, 1, 20).unwrap(), 20);
        for invalid in ["0", "21", "nope"] {
            assert!(parse_u64("LIMIT", Some(invalid), 10, 1, 20).is_err());
        }
    }
}
