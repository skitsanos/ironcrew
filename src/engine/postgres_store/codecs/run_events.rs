use crate::engine::run_events::RunEventGapReason;
use crate::utils::error::{IronCrewError, Result};

pub(in crate::engine::postgres_store) fn run_event_gap_reason_db(
    reason: RunEventGapReason,
) -> &'static str {
    match reason {
        RunEventGapReason::WriterBackpressure => "writer_backpressure",
        RunEventGapReason::Retention => "retention",
        RunEventGapReason::GlobalCapacity => "global_capacity",
        RunEventGapReason::OwnerLost => "owner_lost",
    }
}

pub(in crate::engine::postgres_store) fn parse_run_event_gap_reason(
    value: &str,
) -> Result<RunEventGapReason> {
    match value {
        "writer_backpressure" => Ok(RunEventGapReason::WriterBackpressure),
        "retention" => Ok(RunEventGapReason::Retention),
        "global_capacity" => Ok(RunEventGapReason::GlobalCapacity),
        "owner_lost" => Ok(RunEventGapReason::OwnerLost),
        _ => Err(IronCrewError::Validation(format!(
            "PostgreSQL run-event state contains invalid eviction reason '{value}'"
        ))),
    }
}

pub(in crate::engine::postgres_store) fn nonnegative_u64(label: &str, value: i64) -> Result<u64> {
    u64::try_from(value).map_err(|_| {
        IronCrewError::Validation(format!(
            "PostgreSQL run-event {label} is negative or out of range"
        ))
    })
}

#[cfg(test)]
mod tests {
    use crate::engine::run_events::RunEventGapReason;

    use super::{nonnegative_u64, parse_run_event_gap_reason, run_event_gap_reason_db};

    #[test]
    fn gap_reasons_round_trip_and_numeric_fields_reject_negative_values() {
        for reason in [
            RunEventGapReason::WriterBackpressure,
            RunEventGapReason::Retention,
            RunEventGapReason::GlobalCapacity,
            RunEventGapReason::OwnerLost,
        ] {
            let stored = run_event_gap_reason_db(reason);
            assert_eq!(parse_run_event_gap_reason(stored).unwrap(), reason);
        }
        assert!(parse_run_event_gap_reason("unknown").is_err());
        assert_eq!(nonnegative_u64("sequence", 0).unwrap(), 0);
        assert!(nonnegative_u64("sequence", -1).is_err());
    }
}
