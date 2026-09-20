use serde::de::DeserializeOwned;

use crate::utils::error::{IronCrewError, Result};

mod conversation;
mod idempotency;
mod run_events;
mod runs;

pub(super) use conversation::{
    bounded_conversation_execution, bounded_metadata, bounded_optional_metadata,
    conversation_summary, stored_bytes,
};
pub(super) use idempotency::{
    accounting_row_value, decode_idempotency_accounting, idempotency_record,
};
pub(super) use run_events::{nonnegative_u64, parse_run_event_gap_reason, run_event_gap_reason_db};
pub(super) use runs::{run_record, run_summary};

pub(super) fn decode_stored_json<T: DeserializeOwned>(raw: &str, field: &str) -> Result<T> {
    serde_json::from_str(raw).map_err(|error| {
        IronCrewError::Validation(format!(
            "PostgreSQL stored JSON in '{field}' has an invalid shape: {error}"
        ))
    })
}

pub(super) fn parse_timestamp(label: &str, value: &str) -> Result<chrono::DateTime<chrono::Utc>> {
    chrono::DateTime::parse_from_rfc3339(value)
        .map(|timestamp| timestamp.with_timezone(&chrono::Utc))
        .map_err(|error| {
            IronCrewError::Validation(format!("{label} is not valid RFC3339: {error}"))
        })
}

pub(super) fn canonical_timestamp(label: &str, value: &str) -> Result<String> {
    Ok(parse_timestamp(label, value)?.to_rfc3339_opts(chrono::SecondsFormat::Micros, true))
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use super::{canonical_timestamp, decode_stored_json};

    #[test]
    fn stored_json_errors_retain_the_column_boundary() {
        let error = decode_stored_json::<Value>("not-json", "runs.tags").unwrap_err();
        assert!(error.to_string().contains("runs.tags"));
    }

    #[test]
    fn stored_timestamps_are_canonicalized_to_utc_microseconds() {
        assert_eq!(
            canonical_timestamp("created", "2026-09-20T10:11:12.123+02:00").unwrap(),
            "2026-09-20T08:11:12.123000Z"
        );
        assert!(canonical_timestamp("created", "invalid").is_err());
    }
}
