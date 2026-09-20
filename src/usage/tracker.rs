use std::sync::{Arc, Mutex};

use serde::Serialize;

use super::{UsageAggregate, UsageCoverage, UsageOverflow, UsageReceipt};

/// Process-local accounting scope shared explicitly by clones. No global
/// counter, provider credentials, payloads, request IDs or unbounded ledger.
#[derive(Debug, Clone, Default)]
pub struct UsageTracker(Arc<Mutex<State>>);

#[derive(Debug, Default)]
struct State {
    settled: UsageAggregate,
    in_flight: u64,
    overflowed: bool,
}

/// Settled receipts and outstanding attempts. In-flight counts are not yet in
/// `settled`; they cannot silently turn the scope into complete accounting.
#[derive(Debug, Clone, Serialize)]
pub struct UsageSnapshot {
    pub settled: UsageAggregate,
    pub in_flight: u64,
    pub coverage: UsageCoverage,
}

impl UsageTracker {
    /// Start one actual provider attempt. Hold the guard across request and
    /// stream awaits; give every retry its own guard in the same scope.
    pub fn start(&self) -> Result<UsageAttempt, UsageOverflow> {
        let mut state = self.0.lock().expect("usage lock poisoned");
        if state.overflowed {
            return Err(UsageOverflow);
        }
        state
            .settled
            .requests()
            .checked_add(state.in_flight)
            .and_then(|n| n.checked_add(1))
            .ok_or(UsageOverflow)?;
        state.in_flight += 1;
        Ok(UsageAttempt {
            tracker: Some(self.clone()),
            receipt: UsageReceipt::default(),
        })
    }

    pub fn snapshot(&self) -> Result<UsageSnapshot, UsageOverflow> {
        let state = self.0.lock().expect("usage lock poisoned");
        if state.overflowed {
            return Err(UsageOverflow);
        }
        let coverage = if state.in_flight == 0 {
            state.settled.coverage()
        } else if state.settled.requests() == 0
            || state.settled.coverage() == UsageCoverage::Unavailable
        {
            UsageCoverage::Unavailable
        } else {
            UsageCoverage::Partial
        };
        Ok(UsageSnapshot {
            settled: state.settled.clone(),
            in_flight: state.in_flight,
            coverage,
        })
    }

    fn settle(&self, receipt: &UsageReceipt) -> Result<(), UsageOverflow> {
        let mut state = self.0.lock().expect("usage lock poisoned");
        state.in_flight = state.in_flight.checked_sub(1).expect("live usage attempt");
        if state.overflowed || state.settled.add(receipt).is_err() {
            // Drop cannot return an error. Keep overflow sticky so no later
            // snapshot or request can mistake the pre-overflow state for truth.
            state.overflowed = true;
            return Err(UsageOverflow);
        }
        Ok(())
    }
}

/// Exactly-once settlement by ownership, including ordinary future drop and
/// Tokio cancellation. Process death still requires durable runtime integration.
#[derive(Debug)]
#[must_use = "hold the usage attempt across the provider operation"]
pub struct UsageAttempt {
    tracker: Option<UsageTracker>,
    receipt: UsageReceipt,
}

impl UsageAttempt {
    /// Replace the latest cumulative snapshot. Do not pass per-chunk deltas.
    /// A terminal receipt remains valid even if content processing then fails.
    /// An absent later receipt cannot erase previously observed usage.
    pub fn observe(&mut self, receipt: UsageReceipt) {
        if receipt.coverage() == UsageCoverage::Unavailable
            && self.receipt.coverage() != UsageCoverage::Unavailable
        {
            return;
        }
        self.receipt = receipt;
    }

    pub fn finish(mut self, receipt: UsageReceipt) -> Result<(), UsageOverflow> {
        self.observe(receipt);
        self.settle()
    }

    fn settle(&mut self) -> Result<(), UsageOverflow> {
        match self.tracker.take() {
            Some(tracker) => tracker.settle(&self.receipt),
            None => Ok(()),
        }
    }
}

impl Drop for UsageAttempt {
    fn drop(&mut self) {
        let _ = self.settle();
    }
}
