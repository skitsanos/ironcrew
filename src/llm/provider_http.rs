//! Shared HTTP policy, throttling, error, and SSE primitives for LLM providers.

use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use futures::{Stream, StreamExt};
use reqwest::{ClientBuilder, Response, StatusCode};
use serde_json::Value;
use tokio::sync::Mutex;

use super::execution_policy::ProviderExecutionPolicy;
use crate::utils::error::{IronCrewError, Result};

pub(crate) struct RateLimiter {
    min_interval: Duration,
    last_call: Arc<Mutex<Instant>>,
}

impl RateLimiter {
    pub(crate) fn new(min_interval_ms: u64) -> Self {
        let now = Instant::now();
        Self {
            min_interval: Duration::from_millis(min_interval_ms),
            last_call: Arc::new(Mutex::new(
                now.checked_sub(Duration::from_secs(60)).unwrap_or(now),
            )),
        }
    }

    pub(crate) async fn wait(&self) {
        let mut last = self.last_call.lock().await;
        let elapsed = last.elapsed();
        if elapsed < self.min_interval {
            tokio::time::sleep(self.min_interval - elapsed).await;
        }
        *last = Instant::now();
    }
}

pub(crate) fn secure_provider_client_builder(policy: ProviderExecutionPolicy) -> ClientBuilder {
    crate::utils::network::secure_client_builder(
        crate::utils::network::OutboundNetworkPolicy::PublicOnly,
    )
    .connect_timeout(policy.connect_timeout())
    .timeout(policy.request_timeout())
}

pub(crate) struct ProviderErrorResponse {
    status: StatusCode,
    bytes: Vec<u8>,
}

impl ProviderErrorResponse {
    pub(crate) fn into_error(self) -> IronCrewError {
        let parsed = serde_json::from_slice::<Value>(&self.bytes).ok();
        let root = parsed.as_ref().and_then(|body| {
            body.as_array()
                .and_then(|items| items.first())
                .or(Some(body))
        });
        let message = root
            .and_then(|body| {
                body.pointer("/error/message")
                    .and_then(Value::as_str)
                    .or_else(|| body.get("message").and_then(Value::as_str))
                    .or_else(|| body.get("error").and_then(Value::as_str))
            })
            .map(str::to_owned)
            .unwrap_or_else(|| {
                let raw = String::from_utf8_lossy(&self.bytes);
                let prefix = crate::utils::http::utf8_prefix(raw.trim(), 512);
                if prefix.is_empty() {
                    "provider returned an empty error response".to_owned()
                } else {
                    prefix.to_owned()
                }
            });
        IronCrewError::Provider(format!("HTTP {}: {}", self.status, message))
    }
}

pub(crate) async fn read_error_response(
    response: Response,
    policy: ProviderExecutionPolicy,
    context: &'static str,
) -> Result<ProviderErrorResponse> {
    let status = response.status();
    let bytes = crate::utils::http::read_response_bytes(response, policy.error_bytes(), context)
        .await
        .map_err(|error| IronCrewError::Provider(format!("HTTP {status}: {error}")))?;
    Ok(ProviderErrorResponse { status, bytes })
}

type ResponseStream = Pin<Box<dyn Stream<Item = reqwest::Result<Bytes>> + Send>>;

pub(crate) struct ProviderSseLines {
    stream: ResponseStream,
    buffer: crate::utils::http::BoundedLineBuffer,
    pending: VecDeque<String>,
}

pub(crate) fn sse_field<'a>(line: &'a str, field: &str) -> Option<&'a str> {
    let value = line.strip_prefix(field)?.strip_prefix(':')?;
    Some(value.strip_prefix(' ').unwrap_or(value))
}

impl ProviderSseLines {
    pub(crate) fn new(
        response: Response,
        policy: ProviderExecutionPolicy,
        context: &'static str,
    ) -> Self {
        Self {
            stream: Box::pin(response.bytes_stream()),
            buffer: crate::utils::http::BoundedLineBuffer::new(policy.stream_bytes(), context),
            pending: VecDeque::new(),
        }
    }

    pub(crate) async fn next_line(&mut self) -> Result<Option<String>> {
        loop {
            if let Some(line) = self.pending.pop_front() {
                return Ok(Some(line));
            }
            let Some(chunk) = self.stream.next().await else {
                return Ok(None);
            };
            let chunk = chunk.map_err(IronCrewError::Http)?;
            self.pending = self
                .buffer
                .push(&chunk)
                .map_err(|error| IronCrewError::Provider(error.to_string()))?
                .into();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn rate_limiter_construction_is_underflow_safe() {
        let limiter = RateLimiter::new(1);
        limiter.wait().await;
    }

    #[test]
    fn sse_fields_accept_the_optional_space() {
        assert_eq!(
            sse_field("data:{\"ok\":true}", "data"),
            Some("{\"ok\":true}")
        );
        assert_eq!(sse_field("event: done", "event"), Some("done"));
        assert_eq!(sse_field("data", "data"), None);
    }

    #[test]
    fn non_json_error_keeps_the_http_status() {
        let error = ProviderErrorResponse {
            status: StatusCode::BAD_GATEWAY,
            bytes: b"upstream unavailable".to_vec(),
        }
        .into_error();
        assert_eq!(
            error.to_string(),
            "LLM provider error: HTTP 502 Bad Gateway: upstream unavailable"
        );
    }
}
