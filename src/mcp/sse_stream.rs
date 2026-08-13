//! Pre-parser size and direction gate for HTTP SSE events.

use futures::{StreamExt, stream::BoxStream};
use rmcp::{model::ServerJsonRpcMessage, transport::streamable_http_client::SseError};
use sse_stream::{Sse, SseStream};

use crate::mcp::{
    connection::PoisonWatch,
    http_tool_headers::{HeaderPolicyError, HttpToolHeaderRegistry},
    protocol::inbound_is_allowed,
};

#[derive(Debug, thiserror::Error)]
enum StrictSseError {
    #[error(transparent)]
    Request(#[from] reqwest::Error),
    #[error("MCP connection was closed")]
    Poisoned,
    #[error("MCP SSE event exceeds {0} bytes")]
    EventTooLarge(usize),
    #[error("invalid or prohibited MCP SSE data")]
    ProtocolDirection,
    #[error(transparent)]
    HeaderPolicy(#[from] HeaderPolicyError),
}

struct RechunkState {
    source: BoxStream<'static, Result<bytes::Bytes, reqwest::Error>>,
    poison: PoisonWatch,
    chunk: bytes::Bytes,
    offset: usize,
    event: Vec<u8>,
    limit: usize,
    line_has_data: bool,
    line_is_comment: bool,
    comment_size: usize,
    pending_cr: bool,
    boundary_pending: bool,
}

impl RechunkState {
    fn push_event_byte(&mut self, byte: u8) -> Result<(), StrictSseError> {
        if self.event.len() == self.limit {
            return Err(StrictSseError::EventTooLarge(self.limit));
        }
        self.event.push(byte);
        Ok(())
    }

    fn count_comment_byte(&mut self) -> Result<(), StrictSseError> {
        if self.comment_size == self.limit {
            return Err(StrictSseError::EventTooLarge(self.limit));
        }
        self.comment_size += 1;
        Ok(())
    }

    fn finish_comment(&mut self) {
        self.line_is_comment = false;
        self.comment_size = 0;
    }

    fn take_complete_event(&mut self) -> bytes::Bytes {
        self.boundary_pending = false;
        bytes::Bytes::from(std::mem::take(&mut self.event))
    }

    fn take_event(&mut self) -> Result<Option<bytes::Bytes>, StrictSseError> {
        while self.offset < self.chunk.len() {
            let byte = self.chunk[self.offset];
            if self.pending_cr {
                self.pending_cr = false;
                if byte == b'\n' {
                    if self.line_is_comment {
                        self.count_comment_byte()?;
                    } else {
                        self.push_event_byte(byte)?;
                    }
                    self.offset += 1;
                    if self.boundary_pending {
                        return Ok(Some(self.take_complete_event()));
                    }
                    self.finish_comment();
                    continue;
                }
                self.finish_comment();
                if self.boundary_pending {
                    return Ok(Some(self.take_complete_event()));
                }
            }
            if !self.line_has_data && !matches!(byte, b'\r' | b'\n') {
                self.line_is_comment = byte == b':';
            }
            if self.line_is_comment {
                self.count_comment_byte()?;
            } else {
                self.push_event_byte(byte)?;
            }
            self.offset += 1;
            match byte {
                b'\r' => {
                    self.pending_cr = true;
                    self.boundary_pending = !self.line_has_data;
                    self.line_has_data = false;
                }
                b'\n' if !self.line_has_data => {
                    return Ok(Some(self.take_complete_event()));
                }
                b'\n' => {
                    self.line_has_data = false;
                    self.finish_comment();
                }
                _ => self.line_has_data = true,
            }
        }
        Ok(None)
    }
}

pub(super) fn bounded_sse(
    response: reqwest::Response,
    limit: usize,
    poison: PoisonWatch,
    listing: Option<(HttpToolHeaderRegistry, Option<String>)>,
) -> BoxStream<'static, Result<Sse, SseError>> {
    let state = RechunkState {
        source: response.bytes_stream().boxed(),
        poison: poison.clone(),
        chunk: bytes::Bytes::new(),
        offset: 0,
        event: Vec::new(),
        limit,
        line_has_data: false,
        line_is_comment: false,
        comment_size: 0,
        pending_cr: false,
        boundary_pending: false,
    };
    let bytes = futures::stream::try_unfold(state, |mut state| async move {
        loop {
            if let Some(event) = state.take_event()? {
                return Ok(Some((event, state)));
            }
            let next = tokio::select! {
                biased;
                _ = state.poison.poisoned() => return Err(StrictSseError::Poisoned),
                next = state.source.next() => next,
            };
            match next {
                Some(chunk) => {
                    state.chunk = chunk.map_err(StrictSseError::Request)?;
                    state.offset = 0;
                }
                None if state.boundary_pending => {
                    let event = state.take_complete_event();
                    return Ok(Some((event, state)));
                }
                None if state.pending_cr && state.line_is_comment => {
                    state.pending_cr = false;
                    state.finish_comment();
                    if state.event.is_empty() {
                        return Ok(None);
                    }
                    return Err(StrictSseError::ProtocolDirection);
                }
                None if state.event.is_empty() => return Ok(None),
                None => return Err(StrictSseError::ProtocolDirection),
            }
        }
    });
    SseStream::from_bytes_stream(bytes)
        .map(
            move |event| match event.and_then(|event| validate_event(event, listing.as_ref())) {
                Ok(event) => Ok(event),
                Err(error) => {
                    poison.poison();
                    Err(error)
                }
            },
        )
        .boxed()
}

fn validate_event(
    mut event: Sse,
    listing: Option<&(HttpToolHeaderRegistry, Option<String>)>,
) -> Result<Sse, SseError> {
    let data = event.data.as_deref().filter(|data| !data.trim().is_empty());
    let mut message = data
        .and_then(|data| serde_json::from_str::<ServerJsonRpcMessage>(data).ok())
        .ok_or_else(|| SseError::Body(Box::new(StrictSseError::ProtocolDirection)))?;
    if !inbound_is_allowed(&message) {
        return Err(SseError::Body(Box::new(StrictSseError::ProtocolDirection)));
    }
    if let Some((registry, cursor)) = listing {
        registry
            .stage_pending_server_message(cursor.as_deref(), &mut message)
            .map_err(StrictSseError::HeaderPolicy)
            .map_err(|error| SseError::Body(Box::new(error)))?;
        event.data = Some(
            serde_json::to_string(&message)
                .map_err(|_| SseError::Body(Box::new(StrictSseError::ProtocolDirection)))?,
        );
    }
    Ok(event)
}
