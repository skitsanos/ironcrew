use std::collections::BTreeMap;

use serde_json::Value;

use crate::llm::provider::{ToolCallFunction, ToolCallRequest};
use crate::utils::error::{IronCrewError, Result};

#[derive(Default)]
struct PendingToolCall {
    id: String,
    name: String,
    arguments: String,
    announced: bool,
}

pub(super) struct ToolCallUpdates {
    pub(super) start: Option<(String, String)>,
    pub(super) arguments: Option<(String, String)>,
}

#[derive(Default)]
pub(super) struct StreamToolCalls {
    pending: BTreeMap<usize, PendingToolCall>,
}

impl StreamToolCalls {
    pub(super) fn apply_delta(
        &mut self,
        delta: &Value,
        stored_bytes: &mut usize,
        byte_limit: usize,
    ) -> Result<ToolCallUpdates> {
        let index = delta["index"].as_u64().unwrap_or(0) as usize;
        let pending = self.pending.entry(index).or_default();

        if let Some(id) = delta["id"].as_str()
            && pending.id.is_empty()
        {
            bounded_push(&mut pending.id, id, stored_bytes, byte_limit)?;
        }
        if let Some(name) = delta["function"]["name"].as_str()
            && pending.name.is_empty()
        {
            bounded_push(&mut pending.name, name, stored_bytes, byte_limit)?;
        }

        let start = if !pending.announced && !pending.id.is_empty() && !pending.name.is_empty() {
            pending.announced = true;
            Some((pending.id.clone(), pending.name.clone()))
        } else {
            None
        };
        let arguments = if let Some(value) = delta["function"]["arguments"].as_str() {
            bounded_push(&mut pending.arguments, value, stored_bytes, byte_limit)?;
            Some((pending.id.clone(), value.to_owned()))
        } else {
            None
        };
        Ok(ToolCallUpdates { start, arguments })
    }

    pub(super) fn finish(self) -> Result<Vec<ToolCallRequest>> {
        self.pending
            .into_iter()
            .map(|(index, pending)| {
                if pending.id.is_empty() || pending.name.is_empty() {
                    return Err(IronCrewError::Provider(format!(
                        "OpenAI stream ended with incomplete tool call at index {index}"
                    )));
                }
                Ok(ToolCallRequest {
                    id: pending.id,
                    call_type: "function".to_owned(),
                    function: ToolCallFunction {
                        name: pending.name,
                        arguments: pending.arguments,
                    },
                })
            })
            .collect()
    }
}

fn bounded_push(
    output: &mut String,
    value: &str,
    stored_bytes: &mut usize,
    byte_limit: usize,
) -> Result<()> {
    crate::utils::http::bounded_push_str(
        output,
        value,
        stored_bytes,
        byte_limit,
        "OpenAI accumulated output",
    )
    .map_err(|error| IronCrewError::Provider(error.to_string()))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn separated_name_and_id_are_assembled_and_announced_once() {
        let mut calls = StreamToolCalls::default();
        let mut used = 0;
        let first = calls
            .apply_delta(
                &json!({"index": 0, "function": {"name": "search"}}),
                &mut used,
                1024,
            )
            .unwrap();
        assert!(first.start.is_none());
        let second = calls
            .apply_delta(&json!({"index": 0, "id": "call-1"}), &mut used, 1024)
            .unwrap();
        assert_eq!(second.start, Some(("call-1".into(), "search".into())));
    }

    #[test]
    fn completion_is_index_ordered_and_incomplete_entries_fail() {
        let mut calls = StreamToolCalls::default();
        let mut used = 0;
        for delta in [
            json!({"index": 2, "id": "third", "function": {"name": "c"}}),
            json!({"index": 0, "id": "first", "function": {"name": "a"}}),
        ] {
            calls.apply_delta(&delta, &mut used, 1024).unwrap();
        }
        let completed = calls.finish().unwrap();
        assert_eq!(completed[0].id, "first");
        assert_eq!(completed[1].id, "third");

        let mut incomplete = StreamToolCalls::default();
        incomplete
            .apply_delta(&json!({"index": 1, "id": "missing-name"}), &mut 0, 1024)
            .unwrap();
        assert!(incomplete.finish().is_err());
    }
}
