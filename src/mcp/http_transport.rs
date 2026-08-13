//! Strict MCP 2026 HTTP backend with bounded bodies and no resume/session paths.

use std::{collections::HashMap, sync::Arc};

use crate::mcp::connection::PoisonWatch;
use crate::mcp::http_body::bounded_json;
use crate::mcp::http_tool_headers::{HeaderPolicyError, HttpToolHeaderRegistry};
use crate::mcp::sse_stream::bounded_sse;
use rmcp::{
    model::{ClientJsonRpcMessage, ClientRequest, ServerJsonRpcMessage},
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
    tool_headers: HttpToolHeaderRegistry,
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
    #[error(transparent)]
    HeaderPolicy(#[from] HeaderPolicyError),
}

impl Strict2026HttpClient {
    pub(super) fn new(
        inner: reqwest::Client,
        max_inbound_bytes: usize,
        poison: PoisonWatch,
        tool_headers: HttpToolHeaderRegistry,
    ) -> Self {
        Self {
            inner,
            max_inbound_bytes,
            poison,
            tool_headers,
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

    fn list_cursor(message: &ClientJsonRpcMessage) -> Option<Option<String>> {
        let ClientJsonRpcMessage::Request(request) = message else {
            return None;
        };
        let ClientRequest::ListToolsRequest(list) = &request.request else {
            return None;
        };
        Some(
            list.params
                .as_ref()
                .and_then(|params| params.cursor.clone()),
        )
    }

    fn fatal<T>(
        &self,
        error: StreamableHttpError<StrictHttpError>,
    ) -> Result<T, StreamableHttpError<StrictHttpError>> {
        self.poison.poison();
        Err(error)
    }

    fn fatal_client<T>(
        &self,
        error: StrictHttpError,
    ) -> Result<T, StreamableHttpError<StrictHttpError>> {
        self.fatal(StreamableHttpError::Client(error))
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
                || name.as_str().to_ascii_lowercase().starts_with("mcp-param-")
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
            result = future => match result {
                Ok(value) => Ok(value),
                Err(error) => {
                    self.poison.poison();
                    Err(StrictHttpError::Request(error))
                }
            },
        }
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
        if let Err(error) = Self::validate_outbound(&message) {
            return self.fatal_client(error);
        }
        let list_cursor = Self::list_cursor(&message);
        if session_id.is_some() {
            return self.fatal_client(StrictHttpError::ProtocolDirection);
        }
        if self.poison.is_poisoned() {
            return Err(StreamableHttpError::Client(StrictHttpError::Poisoned));
        }
        let promoted_headers = self
            .tool_headers
            .headers_for_message(&message)
            .map_err(StrictHttpError::HeaderPolicy)
            .map_err(StreamableHttpError::Client)?;
        let mut request = self.inner.post(uri.as_ref()).header(
            reqwest::header::ACCEPT,
            [EVENT_STREAM_MIME_TYPE, JSON_MIME_TYPE].join(", "),
        );
        if let Some(token) = auth_token {
            request = request.bearer_auth(token);
        }
        let mut request = match Self::apply_headers(request, custom_headers) {
            Ok(request) => request,
            Err(error) => return self.fatal_client(error),
        };
        for (name, value) in promoted_headers {
            request = request.header(name, value);
        }
        let response = self
            .poisonable(request.json(&message).send())
            .await
            .map_err(StreamableHttpError::Client)?;
        let status = response.status();
        if response.headers().contains_key(HEADER_SESSION_ID) {
            return self.fatal_client(StrictHttpError::ProtocolDirection);
        }
        if matches!(
            status,
            reqwest::StatusCode::ACCEPTED | reqwest::StatusCode::NO_CONTENT
        ) {
            return self.fatal_client(StrictHttpError::ProtocolDirection);
        }
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        match content_type.as_deref() {
            Some(value) if Self::media_is(value, JSON_MIME_TYPE) => {
                bounded_json(response, self.max_inbound_bytes, self.poison.clone())
                    .await
                    .and_then(|mut message| {
                        if let Some(error) = list_cursor.as_ref().and_then(|cursor| {
                            self.tool_headers
                                .stage_pending_server_message(cursor.as_deref(), &mut message)
                                .err()
                        }) {
                            return self.fatal_client(StrictHttpError::HeaderPolicy(error));
                        }
                        match (&message, status.is_success()) {
                            (ServerJsonRpcMessage::Response(_), true)
                            | (ServerJsonRpcMessage::Error(_), _) => {
                                Ok(StreamableHttpPostResponse::Json(message, None))
                            }
                            _ => self.fatal_client(StrictHttpError::ProtocolDirection),
                        }
                    })
            }
            Some(value) if status.is_success() && Self::media_is(value, EVENT_STREAM_MIME_TYPE) => {
                Ok(StreamableHttpPostResponse::Sse(
                    bounded_sse(
                        response,
                        self.max_inbound_bytes.min(max_sse_event_size),
                        self.poison.clone(),
                        list_cursor.map(|cursor| (self.tool_headers.clone(), cursor)),
                    ),
                    None,
                ))
            }
            _ if !status.is_success() => self.fatal(StreamableHttpError::UnexpectedServerResponse(
                format!("HTTP {status}").into(),
            )),
            _ => self.fatal(StreamableHttpError::UnexpectedContentType(content_type)),
        }
    }

    async fn delete_session(
        &self,
        _uri: Arc<str>,
        _session_id: Arc<str>,
        _auth_token: Option<String>,
        _custom_headers: HashMap<axum::http::HeaderName, axum::http::HeaderValue>,
    ) -> Result<(), StreamableHttpError<Self::Error>> {
        self.fatal_client(StrictHttpError::ProtocolDirection)
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
        self.fatal_client(StrictHttpError::ProtocolDirection)
    }
}
