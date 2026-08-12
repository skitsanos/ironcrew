use futures::StreamExt;
use std::collections::HashMap;
use std::io::Write;

pub const DEFAULT_HTTP_TOOL_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
pub const DEFAULT_HTTP_HEADER_BYTES: usize = 64 * 1024;
pub const DEFAULT_HTTP_TOOL_OUTPUT_BYTES: usize = 16 * 1024 * 1024;
pub const DEFAULT_HTTP_JSON_PARSE_BYTES: usize = 2 * 1024 * 1024;
pub const DEFAULT_WEB_SCRAPE_BYTES: usize = 2 * 1024 * 1024;
pub const DEFAULT_IMAGE_BYTES: usize = 20 * 1024 * 1024;
pub const DEFAULT_PROVIDER_RESPONSE_BYTES: usize = 16 * 1024 * 1024;
pub const DEFAULT_PROVIDER_ERROR_BYTES: usize = 256 * 1024;
pub const DEFAULT_PROVIDER_STREAM_BYTES: usize = 32 * 1024 * 1024;
pub const DEFAULT_PROVIDER_OUTPUT_BYTES: usize = 16 * 1024 * 1024;

const ABSOLUTE_BODY_LIMIT: usize = 256 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum BoundedBodyError {
    #[error("{context} declares {actual} bytes, exceeding the {limit}-byte limit")]
    ContentLength {
        context: &'static str,
        actual: u64,
        limit: usize,
    },
    #[error("{context} exceeded the {limit}-byte limit while streaming")]
    LimitExceeded { context: &'static str, limit: usize },
    #[error("failed to read {context}: {source}")]
    Transport {
        context: &'static str,
        #[source]
        source: reqwest::Error,
    },
    #[error("{context} was not valid UTF-8")]
    InvalidUtf8 { context: &'static str },
}

/// Read a response without ever allocating beyond the configured body budget.
/// Both declared Content-Length and chunked/streaming responses are enforced.
pub async fn read_response_bytes(
    response: reqwest::Response,
    limit: usize,
    context: &'static str,
) -> Result<Vec<u8>, BoundedBodyError> {
    let limit = limit.min(ABSOLUTE_BODY_LIMIT);
    if let Some(actual) = response.content_length()
        && actual > limit as u64
    {
        return Err(BoundedBodyError::ContentLength {
            context,
            actual,
            limit,
        });
    }

    let capacity = response
        .content_length()
        .and_then(|length| usize::try_from(length).ok())
        .unwrap_or(0)
        .min(limit);
    let mut bytes = Vec::with_capacity(capacity);
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|source| BoundedBodyError::Transport { context, source })?;
        let next_len = bytes
            .len()
            .checked_add(chunk.len())
            .ok_or(BoundedBodyError::LimitExceeded { context, limit })?;
        if next_len > limit {
            return Err(BoundedBodyError::LimitExceeded { context, limit });
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

/// Parse a positive byte-limit environment variable with a process hard cap.
/// Invalid, zero, and excessive values fall back to the secure default.
pub fn byte_limit_from_env(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| (1..=ABSOLUTE_BODY_LIMIT).contains(value))
        .unwrap_or(default.min(ABSOLUTE_BODY_LIMIT))
}

/// Like [`byte_limit_from_env`], while retaining an older variable name as a
/// compatibility fallback.
pub fn byte_limit_from_env_with_legacy(name: &str, legacy_name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| (1..=ABSOLUTE_BODY_LIMIT).contains(value))
        .or_else(|| {
            std::env::var(legacy_name)
                .ok()
                .and_then(|value| value.parse::<usize>().ok())
                .filter(|value| (1..=ABSOLUTE_BODY_LIMIT).contains(value))
        })
        .unwrap_or(default.min(ABSOLUTE_BODY_LIMIT))
}

/// Charge an allocation against a shared budget before appending to it.
pub fn bounded_push_str(
    destination: &mut String,
    value: &str,
    used: &mut usize,
    limit: usize,
    context: &'static str,
) -> Result<(), BoundedBodyError> {
    let next = used
        .checked_add(value.len())
        .ok_or(BoundedBodyError::LimitExceeded { context, limit })?;
    if next > limit {
        return Err(BoundedBodyError::LimitExceeded { context, limit });
    }
    destination.push_str(value);
    *used = next;
    Ok(())
}

/// Copy response headers under an aggregate byte budget. Counting happens on
/// raw bytes even when a value is not representable as UTF-8.
pub fn collect_response_headers(
    headers: &reqwest::header::HeaderMap,
    limit: usize,
    context: &'static str,
) -> Result<HashMap<String, String>, BoundedBodyError> {
    let limit = limit.min(ABSOLUTE_BODY_LIMIT);
    let mut used = 0_usize;
    let mut output = HashMap::new();
    for (name, value) in headers {
        used = used
            .checked_add(name.as_str().len())
            .and_then(|next| next.checked_add(value.as_bytes().len()))
            .ok_or(BoundedBodyError::LimitExceeded { context, limit })?;
        if used > limit {
            return Err(BoundedBodyError::LimitExceeded { context, limit });
        }
        if let Ok(value) = value.to_str() {
            output.insert(name.to_string(), value.to_owned());
        }
    }
    Ok(output)
}

struct LimitedWriter {
    bytes: Vec<u8>,
    limit: usize,
}

impl Write for LimitedWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let next = self
            .bytes
            .len()
            .checked_add(bytes.len())
            .ok_or_else(|| std::io::Error::other("serialized output length overflow"))?;
        if next > self.limit {
            return Err(std::io::Error::new(
                std::io::ErrorKind::FileTooLarge,
                format!("serialized output exceeds the {}-byte limit", self.limit),
            ));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Serialize JSON into a bounded writer so escaping/pretty-print expansion
/// cannot create an allocation many times larger than the response body cap.
pub fn to_json_pretty_limited<T: serde::Serialize>(
    value: &T,
    limit: usize,
) -> Result<String, serde_json::Error> {
    let mut writer = LimitedWriter {
        bytes: Vec::new(),
        limit: limit.min(ABSOLUTE_BODY_LIMIT),
    };
    serde_json::to_writer_pretty(&mut writer, value)?;
    // serde_json only writes UTF-8; avoid a second lossy-copy allocation.
    Ok(String::from_utf8(writer.bytes).expect("serde_json emitted valid UTF-8"))
}

/// Byte-buffered line decoder for SSE streams. Buffering bytes until a newline
/// avoids corrupting a UTF-8 code point split across network chunks.
pub struct BoundedLineBuffer {
    buffer: Vec<u8>,
    total: usize,
    limit: usize,
    context: &'static str,
}

impl BoundedLineBuffer {
    pub fn new(limit: usize, context: &'static str) -> Self {
        Self {
            buffer: Vec::new(),
            total: 0,
            limit: limit.min(ABSOLUTE_BODY_LIMIT),
            context,
        }
    }

    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<String>, BoundedBodyError> {
        let next = self
            .total
            .checked_add(chunk.len())
            .ok_or(BoundedBodyError::LimitExceeded {
                context: self.context,
                limit: self.limit,
            })?;
        if next > self.limit {
            return Err(BoundedBodyError::LimitExceeded {
                context: self.context,
                limit: self.limit,
            });
        }
        self.total = next;
        self.buffer.extend_from_slice(chunk);

        let mut lines = Vec::new();
        let mut line_start = 0;
        for (index, byte) in self.buffer.iter().enumerate() {
            if *byte != b'\n' {
                continue;
            }
            let mut line_end = index;
            if line_end > line_start && self.buffer[line_end - 1] == b'\r' {
                line_end -= 1;
            }
            let line = std::str::from_utf8(&self.buffer[line_start..line_end]).map_err(|_| {
                BoundedBodyError::InvalidUtf8 {
                    context: self.context,
                }
            })?;
            lines.push(line.to_owned());
            line_start = index + 1;
        }
        if line_start > 0 {
            self.buffer.drain(..line_start);
        }
        Ok(lines)
    }
}

/// Return a prefix no longer than `max_bytes` without splitting a UTF-8 scalar.
pub fn utf8_prefix(value: &str, max_bytes: usize) -> &str {
    if value.len() <= max_bytes {
        return value;
    }
    let mut end = max_bytes;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn one_shot_server(response: &'static [u8]) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test server");
        let address = listener.local_addr().expect("test address");
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept test request");
            let mut request = [0_u8; 1024];
            let _ = socket.read(&mut request).await;
            socket.write_all(response).await.expect("write response");
        });
        format!("http://{address}/")
    }

    #[tokio::test]
    async fn rejects_oversized_content_length_before_reading() {
        let url = one_shot_server(
            b"HTTP/1.1 200 OK\r\nContent-Length: 1000\r\nConnection: close\r\n\r\nsmall",
        )
        .await;
        let response = reqwest::get(url).await.expect("request test server");
        let error = read_response_bytes(response, 8, "test response")
            .await
            .expect_err("oversized content length must fail");
        assert!(matches!(error, BoundedBodyError::ContentLength { .. }));
    }

    #[tokio::test]
    async fn rejects_oversized_chunked_body_while_streaming() {
        let url = one_shot_server(
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n",
        )
        .await;
        let response = reqwest::get(url).await.expect("request test server");
        let error = read_response_bytes(response, 8, "test response")
            .await
            .expect_err("oversized chunked body must fail");
        assert!(matches!(error, BoundedBodyError::LimitExceeded { .. }));
    }

    #[test]
    fn line_buffer_preserves_utf8_split_across_chunks() {
        let mut buffer = BoundedLineBuffer::new(32, "test stream");
        assert!(buffer.push(&[b'd', 0xe2]).unwrap().is_empty());
        let lines = buffer.push(&[0x82, 0xac, b'\n']).unwrap();
        assert_eq!(lines, vec!["d€"]);
    }

    #[test]
    fn utf8_prefix_never_splits_a_character() {
        assert_eq!(utf8_prefix("hello € world", 8), "hello ");
        assert_eq!(utf8_prefix("hello € world", 9), "hello €");
    }

    #[test]
    fn bounded_json_writer_rejects_escape_expansion() {
        let value = serde_json::json!({"body": "\u{0}".repeat(100)});
        let error = to_json_pretty_limited(&value, 64).expect_err("output must be capped");
        assert!(error.to_string().contains("64-byte limit"));
    }
}
