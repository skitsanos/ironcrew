//! Allocation-free structural checks for untrusted conversation JSON.

use crate::utils::error::{IronCrewError, Result};

use super::conversation_record::{
    HARD_STORED_CONVERSATION_EXECUTION_BYTES, HARD_STORED_CONVERSATION_MESSAGES_BYTES,
};

mod number;
#[cfg(test)]
mod tests;

/// Maximum nesting accepted before typed JSON construction.
pub const HARD_STORED_CONVERSATION_JSON_DEPTH: usize = 64;
/// Maximum JSON values and object keys accepted in one stored conversation.
pub const HARD_STORED_CONVERSATION_JSON_NODES: usize = 262_144;
/// Maximum members in one object or elements in one array.
pub const HARD_STORED_CONVERSATION_JSON_CONTAINER_ENTRIES: usize = 16_384;
/// Maximum encoded bytes in one stored conversation JSON string.
pub const HARD_STORED_CONVERSATION_JSON_STRING_BYTES: usize =
    HARD_STORED_CONVERSATION_MESSAGES_BYTES;

#[derive(Clone, Copy)]
enum Root {
    Object,
    Array,
}

#[derive(Clone, Copy)]
struct Limits {
    bytes: usize,
    depth: usize,
    nodes: usize,
    string_bytes: usize,
    container_entries: usize,
}

impl Limits {
    fn stored(bytes: usize) -> Self {
        Self {
            bytes,
            depth: HARD_STORED_CONVERSATION_JSON_DEPTH,
            nodes: HARD_STORED_CONVERSATION_JSON_NODES,
            string_bytes: bytes.min(HARD_STORED_CONVERSATION_JSON_STRING_BYTES),
            container_entries: HARD_STORED_CONVERSATION_JSON_CONTAINER_ENTRIES,
        }
    }
}

pub fn preflight_conversation_record_json(raw: &str) -> Result<()> {
    preflight_stored(
        raw,
        "record",
        Root::Object,
        HARD_STORED_CONVERSATION_MESSAGES_BYTES,
    )
}

pub fn preflight_conversation_execution_json(raw: &str) -> Result<()> {
    preflight_stored(
        raw,
        "execution identity",
        Root::Object,
        HARD_STORED_CONVERSATION_EXECUTION_BYTES,
    )
}

pub fn preflight_conversation_messages_json(raw: &str) -> Result<()> {
    preflight_stored(
        raw,
        "messages",
        Root::Array,
        HARD_STORED_CONVERSATION_MESSAGES_BYTES,
    )
}

fn preflight_stored(raw: &str, label: &'static str, root: Root, bytes: usize) -> Result<()> {
    preflight(raw, label, root, Limits::stored(bytes))
}

fn preflight(raw: &str, label: &'static str, root: Root, limits: Limits) -> Result<()> {
    if raw.len() > limits.bytes {
        return Err(limit_error(label, "byte"));
    }
    let mut parser = Parser {
        bytes: raw.as_bytes(),
        index: 0,
        nodes: 0,
        label,
        limits,
    };
    parser.skip_whitespace();
    let expected = match root {
        Root::Object => b'{',
        Root::Array => b'[',
    };
    if parser.peek() != Some(expected) {
        return Err(invalid_error(label, "has the wrong top-level shape"));
    }
    parser.parse_value(1)?;
    parser.skip_whitespace();
    if parser.index != parser.bytes.len() {
        return Err(invalid_error(label, "has trailing JSON data"));
    }
    Ok(())
}

struct Parser<'a> {
    bytes: &'a [u8],
    index: usize,
    nodes: usize,
    label: &'static str,
    limits: Limits,
}

impl Parser<'_> {
    fn parse_value(&mut self, depth: usize) -> Result<()> {
        if depth > self.limits.depth {
            return Err(limit_error(self.label, "nesting-depth"));
        }
        self.bump_node()?;
        match self.peek() {
            Some(b'{') => self.parse_object(depth),
            Some(b'[') => self.parse_array(depth),
            Some(b'"') => self.parse_string(),
            Some(b't') => self.parse_literal(b"true"),
            Some(b'f') => self.parse_literal(b"false"),
            Some(b'n') => self.parse_literal(b"null"),
            Some(b'-' | b'0'..=b'9') => {
                self.index = number::scan(self.bytes, self.index)
                    .ok_or_else(|| invalid_error(self.label, "contains an invalid JSON number"))?;
                Ok(())
            }
            _ => Err(invalid_error(self.label, "contains invalid JSON")),
        }
    }

    fn parse_object(&mut self, depth: usize) -> Result<()> {
        self.index += 1;
        self.skip_whitespace();
        if self.take(b'}') {
            return Ok(());
        }
        let mut entries = 0usize;
        loop {
            entries = entries.saturating_add(1);
            if entries > self.limits.container_entries {
                return Err(limit_error(self.label, "container-entry"));
            }
            self.bump_node()?;
            if self.peek() != Some(b'"') {
                return Err(invalid_error(
                    self.label,
                    "contains a non-string object key",
                ));
            }
            self.parse_string()?;
            self.skip_whitespace();
            if !self.take(b':') {
                return Err(invalid_error(self.label, "contains invalid object syntax"));
            }
            self.skip_whitespace();
            self.parse_value(depth.saturating_add(1))?;
            self.skip_whitespace();
            if self.take(b'}') {
                return Ok(());
            }
            if !self.take(b',') {
                return Err(invalid_error(self.label, "contains invalid object syntax"));
            }
            self.skip_whitespace();
        }
    }

    fn parse_array(&mut self, depth: usize) -> Result<()> {
        self.index += 1;
        self.skip_whitespace();
        if self.take(b']') {
            return Ok(());
        }
        let mut entries = 0usize;
        loop {
            entries = entries.saturating_add(1);
            if entries > self.limits.container_entries {
                return Err(limit_error(self.label, "container-entry"));
            }
            self.parse_value(depth.saturating_add(1))?;
            self.skip_whitespace();
            if self.take(b']') {
                return Ok(());
            }
            if !self.take(b',') {
                return Err(invalid_error(self.label, "contains invalid array syntax"));
            }
            self.skip_whitespace();
        }
    }

    fn parse_string(&mut self) -> Result<()> {
        self.index += 1;
        let start = self.index;
        while let Some(byte) = self.peek() {
            match byte {
                b'"' => {
                    self.index += 1;
                    return Ok(());
                }
                b'\\' => {
                    self.index += 1;
                    let escape = self.peek().ok_or_else(|| {
                        invalid_error(self.label, "contains an unterminated string escape")
                    })?;
                    self.index += 1;
                    if escape == b'u' {
                        for _ in 0..4 {
                            if !self.peek().is_some_and(|value| value.is_ascii_hexdigit()) {
                                return Err(invalid_error(
                                    self.label,
                                    "contains an invalid Unicode escape",
                                ));
                            }
                            self.index += 1;
                        }
                    } else if !matches!(
                        escape,
                        b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't'
                    ) {
                        return Err(invalid_error(
                            self.label,
                            "contains an invalid string escape",
                        ));
                    }
                }
                0x00..=0x1f => {
                    return Err(invalid_error(
                        self.label,
                        "contains a control byte in a string",
                    ));
                }
                _ => self.index += 1,
            }
            if self.index.saturating_sub(start) > self.limits.string_bytes {
                return Err(limit_error(self.label, "string-byte"));
            }
        }
        Err(invalid_error(self.label, "contains an unterminated string"))
    }

    fn parse_literal(&mut self, literal: &[u8]) -> Result<()> {
        if self
            .bytes
            .get(self.index..self.index.saturating_add(literal.len()))
            != Some(literal)
        {
            return Err(invalid_error(
                self.label,
                "contains an invalid JSON literal",
            ));
        }
        self.index += literal.len();
        Ok(())
    }

    fn bump_node(&mut self) -> Result<()> {
        self.nodes = self.nodes.saturating_add(1);
        if self.nodes > self.limits.nodes {
            return Err(limit_error(self.label, "node"));
        }
        Ok(())
    }

    fn skip_whitespace(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\n' | b'\r' | b'\t')) {
            self.index += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.index).copied()
    }

    fn take(&mut self, expected: u8) -> bool {
        if self.peek() == Some(expected) {
            self.index += 1;
            true
        } else {
            false
        }
    }
}

fn limit_error(label: &str, limit: &str) -> IronCrewError {
    IronCrewError::Validation(format!(
        "Stored conversation {label} exceeds the hard JSON {limit} limit"
    ))
}

fn invalid_error(label: &str, detail: &str) -> IronCrewError {
    IronCrewError::Validation(format!("Stored conversation {label} {detail}"))
}
