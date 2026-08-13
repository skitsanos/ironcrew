//! Pre-parser size and direction gate for HTTP SSE events.

use futures::{StreamExt, stream::BoxStream};
use rmcp::{model::ServerJsonRpcMessage, transport::streamable_http_client::SseError};
use sse_stream::{Sse, SseStream};

use crate::mcp::{connection::PoisonWatch, protocol::inbound_is_allowed};

#[derive(Debug, thiserror::Error)]
enum StrictSseError {
    #[error(transparent)]
    Request(#[from] reqwest::Error),
    #[error("MCP connection was closed")]
    Poisoned,
    #[error("MCP SSE event exceeds {0} bytes")]
    EventTooLarge(usize),
    #[error("SSE message is outside IronCrew's strict MCP 2026 surface")]
    ProtocolDirection,
}

#[derive(Default)]
struct EventCounter {
    size: usize,
    tail: [u8; 4],
    tail_len: usize,
}

impl EventCounter {
    fn observe(&mut self, chunk: &[u8], limit: usize) -> Result<(), StrictSseError> {
        for byte in chunk {
            self.size = self.size.saturating_add(1);
            if self.size > limit {
                return Err(StrictSseError::EventTooLarge(limit));
            }
            if self.tail_len < self.tail.len() {
                self.tail[self.tail_len] = *byte;
                self.tail_len += 1;
            } else {
                self.tail.rotate_left(1);
                self.tail[3] = *byte;
            }
            let tail = &self.tail[..self.tail_len];
            if tail.ends_with(b"\n\n") || tail.ends_with(b"\r\r") || tail.ends_with(b"\r\n\r\n") {
                self.size = 0;
                self.tail_len = 0;
            }
        }
        Ok(())
    }
}

pub(super) fn bounded_sse(
    response: reqwest::Response,
    limit: usize,
    poison: PoisonWatch,
) -> BoxStream<'static, Result<Sse, SseError>> {
    let source = response.bytes_stream().boxed();
    let bytes = futures::stream::try_unfold(
        (source, poison, EventCounter::default()),
        move |(mut source, mut poison, mut counter)| async move {
            let next = tokio::select! {
                biased;
                _ = poison.poisoned() => return Err(StrictSseError::Poisoned),
                next = source.next() => next,
            };
            let Some(chunk) = next else { return Ok(None) };
            let chunk = chunk.map_err(StrictSseError::Request)?;
            counter.observe(&chunk, limit)?;
            Ok(Some((chunk, (source, poison, counter))))
        },
    );
    SseStream::from_bytes_stream(bytes)
        .map(|event| match event {
            Ok(event)
                if event.data.as_deref().is_some_and(|data| {
                    serde_json::from_str::<ServerJsonRpcMessage>(data)
                        .is_ok_and(|message| !inbound_is_allowed(&message))
                }) =>
            {
                Err(SseError::Body(Box::new(StrictSseError::ProtocolDirection)))
            }
            other => other,
        })
        .boxed()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counter_resets_only_at_event_boundaries() {
        let mut counter = EventCounter::default();
        counter.observe(b"data: 1\n\n", 9).unwrap();
        counter.observe(b"data: 2\r\n\r\n", 11).unwrap();
        assert!(counter.observe(b"data: too-long", 4).is_err());
    }
}
