//! Strict MCP 2026 HTTP backend with bounded bodies and no resume/session paths.

use std::{collections::HashMap, sync::Arc};

use crate::mcp::connection::PoisonWatch;
use crate::mcp::protocol::inbound_is_allowed;
use crate::mcp::sse_stream::bounded_sse;
use futures::StreamExt;
use rmcp::{
    model::{ClientJsonRpcMessage, ServerJsonRpcMessage},
    transport::{
        common::http_header::{
            EVENT_STREAM_MIME_TYPE, HEADER_LAST_EVENT_ID, HEADER_SESSION_ID, JSON_MIME_TYPE,
        },
        streamable_http_client::{
            SseError, StreamableHttpClient, StreamableHttpError, StreamableHttpPostResponse,
        },
    },
};

#[derive(Clone)]
pub(super) struct Strict2026HttpClient {
    inner: reqwest::Client,
    max_inbound_bytes: usize,
    poison: PoisonWatch,
}

#[derive(Debug, thiserror::Error)]
pub(super) enum StrictHttpError {
    #[error(transparent)]
    Request(#[from] reqwest::Error),
    #[error("MCP connection was closed")]
    Poisoned,
    #[error("MCP HTTP message exceeds {0} bytes")]
    MessageTooLarge(usize),
    #[error("message is outside IronCrew's strict MCP 2026 surface")]
    ProtocolDirection,
}

impl Strict2026HttpClient {
    pub(super) fn new(
        inner: reqwest::Client,
        max_inbound_bytes: usize,
        poison: PoisonWatch,
    ) -> Self {
        Self {
            inner,
            max_inbound_bytes,
            poison,
        }
    }

    fn validate_outbound(message: &ClientJsonRpcMessage) -> Result<(), StrictHttpError> {
        let allowed = match message {
            ClientJsonRpcMessage::Request(request) => matches!(
                request.request.method(),
                "server/discover" | "tools/list" | "tools/call"
            ),
            ClientJsonRpcMessage::Notification(_)
            | ClientJsonRpcMessage::Response(_)
            | ClientJsonRpcMessage::Error(_) => false,
        };
        allowed
            .then_some(())
            .ok_or(StrictHttpError::ProtocolDirection)
    }

    fn apply_headers(
        mut request: reqwest::RequestBuilder,
        headers: HashMap<axum::http::HeaderName, axum::http::HeaderValue>,
    ) -> Result<reqwest::RequestBuilder, StrictHttpError> {
        for (name, value) in headers {
            if [
                "accept",
                "content-type",
                "content-length",
                "transfer-encoding",
                "host",
                HEADER_SESSION_ID,
                HEADER_LAST_EVENT_ID,
            ]
            .iter()
            .any(|reserved| name.as_str().eq_ignore_ascii_case(reserved))
            {
                return Err(StrictHttpError::ProtocolDirection);
            }
            request = request.header(name, value);
        }
        Ok(request)
    }

    async fn poisonable<T>(
        &self,
        future: impl Future<Output = Result<T, reqwest::Error>>,
    ) -> Result<T, StrictHttpError> {
        let mut poison = self.poison.clone();
        if poison.is_poisoned() {
            return Err(StrictHttpError::Poisoned);
        }
        tokio::select! {
            biased;
            _ = poison.poisoned() => Err(StrictHttpError::Poisoned),
            result = future => result.map_err(StrictHttpError::Request),
        }
    }

    async fn bounded_json(
        &self,
        response: reqwest::Response,
    ) -> Result<ServerJsonRpcMessage, StreamableHttpError<StrictHttpError>> {
        if response
            .content_length()
            .is_some_and(|length| length > self.max_inbound_bytes as u64)
        {
            return Err(StreamableHttpError::Client(
                StrictHttpError::MessageTooLarge(self.max_inbound_bytes),
            ));
        }
        let mut stream = response.bytes_stream();
        let mut body = Vec::new();
        let mut poison = self.poison.clone();
        loop {
            let next = tokio::select! {
                biased;
                _ = poison.poisoned() => {
                    return Err(StreamableHttpError::Client(StrictHttpError::Poisoned));
                }
                next = stream.next() => next,
            };
            let Some(chunk) = next else { break };
            let chunk = chunk
                .map_err(|error| StreamableHttpError::Client(StrictHttpError::Request(error)))?;
            if body.len().saturating_add(chunk.len()) > self.max_inbound_bytes {
                return Err(StreamableHttpError::Client(
                    StrictHttpError::MessageTooLarge(self.max_inbound_bytes),
                ));
            }
            body.extend_from_slice(&chunk);
        }
        let message = serde_json::from_slice::<ServerJsonRpcMessage>(&body)
            .map_err(StreamableHttpError::Deserialize)?;
        if !inbound_is_allowed(&message) {
            return Err(StreamableHttpError::Client(
                StrictHttpError::ProtocolDirection,
            ));
        }
        Ok(message)
    }

    fn media_is(value: &str, expected: &str) -> bool {
        value
            .split(';')
            .next()
            .is_some_and(|essence| essence.trim().eq_ignore_ascii_case(expected))
    }
}

impl StreamableHttpClient for Strict2026HttpClient {
    type Error = StrictHttpError;

    async fn post_message(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        auth_token: Option<String>,
        custom_headers: HashMap<axum::http::HeaderName, axum::http::HeaderValue>,
    ) -> Result<StreamableHttpPostResponse, StreamableHttpError<Self::Error>> {
        self.post_message_with_max_sse_event_size(
            uri,
            message,
            session_id,
            auth_token,
            custom_headers,
            self.max_inbound_bytes,
        )
        .await
    }

    async fn post_message_with_max_sse_event_size(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        auth_token: Option<String>,
        custom_headers: HashMap<axum::http::HeaderName, axum::http::HeaderValue>,
        max_sse_event_size: usize,
    ) -> Result<StreamableHttpPostResponse, StreamableHttpError<Self::Error>> {
        Self::validate_outbound(&message).map_err(StreamableHttpError::Client)?;
        if session_id.is_some() || self.poison.is_poisoned() {
            return Err(StreamableHttpError::Client(if session_id.is_some() {
                StrictHttpError::ProtocolDirection
            } else {
                StrictHttpError::Poisoned
            }));
        }
        let mut request = self.inner.post(uri.as_ref()).header(
            reqwest::header::ACCEPT,
            [EVENT_STREAM_MIME_TYPE, JSON_MIME_TYPE].join(", "),
        );
        if let Some(token) = auth_token {
            request = request.bearer_auth(token);
        }
        let request =
            Self::apply_headers(request, custom_headers).map_err(StreamableHttpError::Client)?;
        let response = self
            .poisonable(request.json(&message).send())
            .await
            .map_err(StreamableHttpError::Client)?;
        let status = response.status();
        if response.headers().contains_key(HEADER_SESSION_ID) {
            return Err(StreamableHttpError::Client(
                StrictHttpError::ProtocolDirection,
            ));
        }
        if matches!(
            status,
            reqwest::StatusCode::ACCEPTED | reqwest::StatusCode::NO_CONTENT
        ) {
            return Ok(StreamableHttpPostResponse::Accepted);
        }
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        match content_type.as_deref() {
            Some(value) if Self::media_is(value, JSON_MIME_TYPE) => self
                .bounded_json(response)
                .await
                .and_then(|message| match message {
                    ServerJsonRpcMessage::Response(_) | ServerJsonRpcMessage::Error(_) => {
                        Ok(StreamableHttpPostResponse::Json(message, None))
                    }
                    _ => Err(StreamableHttpError::Client(
                        StrictHttpError::ProtocolDirection,
                    )),
                }),
            Some(value) if status.is_success() && Self::media_is(value, EVENT_STREAM_MIME_TYPE) => {
                Ok(StreamableHttpPostResponse::Sse(
                    bounded_sse(
                        response,
                        self.max_inbound_bytes.min(max_sse_event_size),
                        self.poison.clone(),
                    ),
                    None,
                ))
            }
            _ if !status.is_success() => Err(StreamableHttpError::UnexpectedServerResponse(
                format!("HTTP {status}").into(),
            )),
            _ => Err(StreamableHttpError::UnexpectedContentType(content_type)),
        }
    }

    async fn delete_session(
        &self,
        _uri: Arc<str>,
        _session_id: Arc<str>,
        _auth_token: Option<String>,
        _custom_headers: HashMap<axum::http::HeaderName, axum::http::HeaderValue>,
    ) -> Result<(), StreamableHttpError<Self::Error>> {
        Err(StreamableHttpError::Client(
            StrictHttpError::ProtocolDirection,
        ))
    }

    async fn get_stream(
        &self,
        _uri: Arc<str>,
        _session_id: Option<Arc<str>>,
        _last_event_id: Option<String>,
        _auth_token: Option<String>,
        _custom_headers: HashMap<axum::http::HeaderName, axum::http::HeaderValue>,
    ) -> Result<
        futures::stream::BoxStream<'static, Result<sse_stream::Sse, SseError>>,
        StreamableHttpError<Self::Error>,
    > {
        Err(StreamableHttpError::Client(
            StrictHttpError::ProtocolDirection,
        ))
    }
}
