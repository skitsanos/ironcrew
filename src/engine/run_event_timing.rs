//! Checked timing policy for PostgreSQL run-event journal writes.
//!
//! The Tokio attempt deadline includes pool acquisition and the complete
//! append transaction. PostgreSQL receives a smaller per-statement deadline
//! so a blocked query is cancelled before the outer future is dropped. The
//! acknowledgement deadline covers one complete retry window plus bounded
//! scheduling and rollback headroom; it is intentionally not an unbounded
//! promise to drain every queued event.

use std::time::Duration;

const APPEND_MAX_ATTEMPTS: usize = 3;
const APPEND_RETRY_BACKOFF_BASE: Duration = Duration::from_millis(50);
const DATABASE_TIMEOUT_NUMERATOR: u32 = 4;
const DATABASE_TIMEOUT_DENOMINATOR: u32 = 5;
const ACKNOWLEDGEMENT_MARGIN: Duration = Duration::from_millis(500);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RunEventWriteTiming {
    attempt_timeout: Duration,
    database_timeout: Duration,
    retry_window: Duration,
    acknowledgement_timeout: Duration,
}

impl RunEventWriteTiming {
    pub(crate) fn checked(attempt_timeout: Duration) -> Option<Self> {
        if attempt_timeout.is_zero() {
            return None;
        }
        let database_timeout = attempt_timeout
            .checked_mul(DATABASE_TIMEOUT_NUMERATOR)?
            .checked_div(DATABASE_TIMEOUT_DENOMINATOR)?;
        if database_timeout.is_zero() || database_timeout >= attempt_timeout {
            return None;
        }

        let mut retry_window = attempt_timeout.checked_mul(APPEND_MAX_ATTEMPTS as u32)?;
        for completed_attempt in 1..APPEND_MAX_ATTEMPTS {
            retry_window = retry_window.checked_add(retry_backoff_after(completed_attempt)?)?;
        }
        let acknowledgement_timeout = retry_window.checked_add(ACKNOWLEDGEMENT_MARGIN)?;

        Some(Self {
            attempt_timeout,
            database_timeout,
            retry_window,
            acknowledgement_timeout,
        })
    }

    pub(crate) const fn max_attempts(self) -> usize {
        APPEND_MAX_ATTEMPTS
    }

    pub(crate) const fn attempt_timeout(self) -> Duration {
        self.attempt_timeout
    }

    pub(crate) const fn database_timeout(self) -> Duration {
        self.database_timeout
    }

    pub(crate) const fn acknowledgement_timeout(self) -> Duration {
        self.acknowledgement_timeout
    }

    pub(crate) fn backoff_after(self, completed_attempt: usize) -> Option<Duration> {
        retry_backoff_after(completed_attempt)
    }
}

fn retry_backoff_after(completed_attempt: usize) -> Option<Duration> {
    if completed_attempt == 0 || completed_attempt >= APPEND_MAX_ATTEMPTS {
        return None;
    }
    let shift = u32::try_from(completed_attempt.checked_sub(1)?).ok()?;
    APPEND_RETRY_BACKOFF_BASE.checked_mul(1u32.checked_shl(shift)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_timing_has_database_and_acknowledgement_headroom() {
        let timing = RunEventWriteTiming::checked(Duration::from_millis(1_500)).unwrap();
        assert_eq!(timing.attempt_timeout, Duration::from_millis(1_500));
        assert_eq!(timing.database_timeout, Duration::from_millis(1_200));
        assert_eq!(timing.backoff_after(1), Some(Duration::from_millis(50)));
        assert_eq!(timing.backoff_after(2), Some(Duration::from_millis(100)));
        assert_eq!(timing.backoff_after(3), None);
        assert_eq!(timing.retry_window, Duration::from_millis(4_650));
        assert_eq!(timing.acknowledgement_timeout, Duration::from_millis(5_150));
    }

    #[test]
    fn supported_maximum_is_checked_without_overflow() {
        let timing = RunEventWriteTiming::checked(Duration::from_millis(5_000)).unwrap();
        assert_eq!(timing.database_timeout, Duration::from_millis(4_000));
        assert_eq!(timing.retry_window, Duration::from_millis(15_150));
        assert_eq!(
            timing.acknowledgement_timeout,
            Duration::from_millis(15_650)
        );
    }

    #[test]
    fn zero_and_overflowing_windows_are_rejected() {
        assert!(RunEventWriteTiming::checked(Duration::ZERO).is_none());
        assert!(RunEventWriteTiming::checked(Duration::MAX).is_none());
    }
}
