use super::{UsageAggregate, UsageCoverage, UsageOverflow, UsageReceipt, UsageSnapshot};
use std::sync::{Arc, Mutex};

/// An inclusive scope. Descendants update this scope directly; callers never
/// merge their results back into a parent that already includes them.
#[derive(Debug, Clone, Default)]
pub struct UsageTracker(Arc<Node>);

#[derive(Debug, Default)]
struct Node {
    state: Mutex<State>,
    parent: Option<UsageTracker>,
    depth: usize,
}

#[derive(Debug, Default)]
struct State {
    settled: UsageAggregate,
    in_flight: u64,
    overflowed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("usage scope nesting exceeds the supported depth of 64")]
pub struct UsageScopeDepth;

impl UsageTracker {
    /// Create a disjoint child view while retaining inclusive ancestor totals.
    /// Nesting is bounded and all lineage locks are acquired root-first.
    pub fn child(&self) -> Result<Self, UsageScopeDepth> {
        if self.0.depth >= 64 {
            return Err(UsageScopeDepth);
        }
        Ok(Self(Arc::new(Node {
            state: Mutex::default(),
            parent: Some(self.clone()),
            depth: self.0.depth + 1,
        })))
    }

    fn lineage(&self) -> Vec<&Mutex<State>> {
        let mut nodes = Vec::with_capacity(self.0.depth + 1);
        let mut current = Some(self);
        while let Some(tracker) = current {
            nodes.push(&tracker.0.state);
            current = tracker.0.parent.as_ref();
        }
        nodes.reverse();
        nodes
    }

    /// Start one dispatch, atomically registering it in this scope and each
    /// ancestor. A child cannot hide overflow in its enclosing flow.
    pub fn start(&self) -> Result<UsageAttempt, UsageOverflow> {
        let nodes = self.lineage();
        let mut states = nodes
            .iter()
            .map(|node| node.lock().expect("usage lock poisoned"))
            .collect::<Vec<_>>();
        for state in &states {
            if state.overflowed {
                return Err(UsageOverflow);
            }
            state
                .settled
                .requests()
                .checked_add(state.in_flight)
                .and_then(|n| n.checked_add(1))
                .ok_or(UsageOverflow)?;
        }
        for state in &mut states {
            state.in_flight += 1;
        }
        Ok(UsageAttempt {
            tracker: Some(self.clone()),
            receipt: UsageReceipt::default(),
        })
    }

    pub fn snapshot(&self) -> Result<UsageSnapshot, UsageOverflow> {
        let state = self.0.state.lock().expect("usage lock poisoned");
        if state.overflowed {
            return Err(UsageOverflow);
        }
        Ok(UsageSnapshot::new(state.settled.clone(), state.in_flight))
    }

    fn settle(&self, receipt: &UsageReceipt) -> Result<(), UsageOverflow> {
        let nodes = self.lineage();
        let mut states = nodes
            .iter()
            .map(|node| node.lock().expect("usage lock poisoned"))
            .collect::<Vec<_>>();
        for state in &mut states {
            state.in_flight = state.in_flight.checked_sub(1).expect("live usage attempt");
        }
        let totals = states
            .iter()
            .map(|state| {
                if state.overflowed {
                    return Err(UsageOverflow);
                }
                let mut total = state.settled.clone();
                total.add(receipt)?;
                Ok(total)
            })
            .collect::<Result<Vec<_>, UsageOverflow>>();
        match totals {
            Ok(totals) => {
                for (state, total) in states.iter_mut().zip(totals) {
                    state.settled = total;
                }
                Ok(())
            }
            Err(error) => {
                // No ancestor or child can advertise a partially applied update.
                for state in &mut states {
                    state.overflowed = true;
                }
                Err(error)
            }
        }
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
