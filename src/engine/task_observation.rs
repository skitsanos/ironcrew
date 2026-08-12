//! Fixed-cardinality task-attempt observation.

use std::time::{Duration, Instant};

use crate::metrics::TaskOutcome;

/// Records cancellation when an admitted task future is dropped before it
/// publishes a terminal outcome. This stays outside task correctness paths:
/// every update is a process-local atomic increment.
pub(crate) struct TaskObservation {
    started_at: Instant,
    finished: bool,
}

impl TaskObservation {
    pub(crate) fn start() -> Self {
        Self {
            started_at: Instant::now(),
            finished: false,
        }
    }

    pub(crate) fn finish(mut self, outcome: TaskOutcome) {
        self.finished = true;
        crate::metrics::record_task(outcome, self.started_at.elapsed());
    }

    pub(crate) fn finish_skipped(mut self) {
        self.finished = true;
        record_skipped();
    }
}

impl Drop for TaskObservation {
    fn drop(&mut self) {
        if !self.finished {
            crate::metrics::record_task(TaskOutcome::Cancelled, self.started_at.elapsed());
        }
    }
}

pub(crate) fn record_skipped() {
    crate::metrics::record_task(TaskOutcome::Skipped, Duration::ZERO);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task_total(outcome: &str) -> u64 {
        let mut body = String::new();
        crate::metrics::append_prometheus(&mut body);
        let prefix = format!("ironcrew_tasks_total{{outcome=\"{outcome}\"}} ");
        body.lines()
            .find_map(|line| line.strip_prefix(&prefix)?.parse().ok())
            .unwrap_or_else(|| panic!("missing task outcome {outcome}"))
    }

    #[test]
    fn dropped_observation_records_cancellation() {
        let before = task_total("cancelled");
        drop(TaskObservation::start());
        assert!(task_total("cancelled") > before);
    }

    #[test]
    fn skipped_observation_uses_the_zero_duration_path() {
        let before = task_total("skipped");
        TaskObservation::start().finish_skipped();
        assert!(task_total("skipped") > before);
    }
}
