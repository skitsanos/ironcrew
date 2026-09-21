use super::*;

pub(super) fn classify_work_result(
    join_result: std::result::Result<
        std::result::Result<RunWorkResult, IronCrewError>,
        tokio::task::JoinError,
    >,
    elapsed_ms: u64,
) -> WorkOutcome {
    match join_result {
        Ok(Ok(work)) => {
            let RunWorkResult {
                status: response_status,
                duration_ms: response_duration_ms,
                usage: response_usage,
            } = work;
            let status = response_status
                .parse::<RunStatus>()
                .ok()
                .filter(RunStatus::is_terminal)
                .unwrap_or(RunStatus::Success);
            WorkOutcome {
                status,
                duration_ms: response_duration_ms,
                usage: response_usage,
                error_message: None,
            }
        }
        Ok(Err(error)) => WorkOutcome {
            status: RunStatus::Failed,
            duration_ms: elapsed_ms,
            usage: crate::usage::UsageSnapshot::unavailable(),
            error_message: Some(error.to_string()),
        },
        Err(join_error) if join_error.is_cancelled() => WorkOutcome {
            status: RunStatus::Aborted,
            duration_ms: elapsed_ms,
            usage: crate::usage::UsageSnapshot::unavailable(),
            error_message: None,
        },
        Err(join_error) => WorkOutcome {
            status: RunStatus::Failed,
            duration_ms: elapsed_ms,
            usage: crate::usage::UsageSnapshot::unavailable(),
            error_message: Some(format!("Task panicked: {join_error}")),
        },
    }
}
