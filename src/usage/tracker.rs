use super::{UsageAggregate, UsageCoverage, UsageOverflow, UsageReceipt, UsageSnapshot};
use std::sync::{Arc, Mutex};

/// An inclusive scope. Descendants update this scope directly; callers never
/// merge their results back into a parent that already includes them.
#[derive(Clone, Default)]
pub struct UsageTracker(Arc<Node>);

impl std::fmt::Debug for UsageTracker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UsageTracker")
            .field("depth", &self.0.depth)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Default)]
struct Node {
    budget: super::budget::TokenBudget,
    state: Mutex<State>,
    parent: Option<UsageTracker>,
    observer: Option<UsageTracker>,
    depth: usize,
}

#[derive(Debug, Default)]
struct State {
    settled: UsageAggregate,
    in_flight: u64,
    overflowed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("usage scope ancestry exceeds the supported bounds of 64 levels and 128 nodes")]
pub struct UsageScopeDepth;

impl UsageTracker {
    pub fn for_run() -> Result<Self, super::budget::BudgetError> {
        Ok(Self::with_budget(
            super::budget::TokenBudget::from_environment()?,
        ))
    }

    pub fn with_budget(budget: super::budget::TokenBudget) -> Self {
        Self(Arc::new(Node {
            budget,
            ..Node::default()
        }))
    }

    pub fn budget(&self) -> &super::budget::TokenBudget {
        &self.0.budget
    }

    /// Create a disjoint child view while retaining inclusive ancestor totals.
    /// Nesting is bounded and shared lineage nodes are locked exactly once.
    pub fn child(&self) -> Result<Self, UsageScopeDepth> {
        self.new_child(None)
    }

    /// Observe the same new attempts in a second inclusive scope without
    /// re-adding history. Useful for session totals alongside a caller's run.
    /// Shared ancestors are deduplicated; an independent observer does not
    /// inject its historical receipts into the caller's scope.
    pub fn child_observed_by(&self, observer: &Self) -> Result<Self, UsageScopeDepth> {
        self.new_child(Some(observer.clone()))
    }

    fn new_child(&self, observer: Option<Self>) -> Result<Self, UsageScopeDepth> {
        let depth = self
            .0
            .depth
            .max(observer.as_ref().map_or(0, |scope| scope.0.depth))
            + 1;
        if depth > 64 {
            return Err(UsageScopeDepth);
        }
        let child = Self(Arc::new(Node {
            budget: self.0.budget.clone(),
            state: Mutex::default(),
            parent: Some(self.clone()),
            observer,
            depth,
        }));
        child.ancestry()?;
        Ok(child)
    }

    /// Restore a settled checkpoint into an independent root. Historical
    /// receipts must never be replayed into a newly created run's ancestors.
    pub fn from_snapshot(snapshot: UsageSnapshot) -> Result<Self, &'static str> {
        snapshot.validate()?;
        if snapshot.in_flight != 0 {
            return Err("cannot resume an in-flight usage checkpoint");
        }
        Ok(Self(Arc::new(Node {
            state: Mutex::new(State {
                settled: snapshot.settled,
                ..State::default()
            }),
            ..Node::default()
        })))
    }

    fn ancestry(&self) -> Result<Vec<&Node>, UsageScopeDepth> {
        let mut nodes: Vec<&Node> = Vec::with_capacity(self.0.depth + 1);
        let mut pending = vec![self.0.as_ref()];
        while let Some(node) = pending.pop() {
            if nodes.iter().any(|other| std::ptr::eq(*other, node)) {
                continue;
            }
            if nodes.len() == 128 {
                return Err(UsageScopeDepth);
            }
            nodes.push(node);
            for parent in [&node.parent, &node.observer].into_iter().flatten() {
                pending.push(parent.0.as_ref());
            }
        }
        // A global address order prevents opposite observer links from taking
        // the same immutable DAG's locks in different orders.
        nodes.sort_unstable_by_key(|node| std::ptr::from_ref(*node) as usize);
        Ok(nodes)
    }

    fn lineage(&self) -> Vec<&Mutex<State>> {
        self.ancestry()
            .expect("usage ancestry validated at construction")
            .into_iter()
            .map(|node| &node.state)
            .collect()
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
        let mut snapshot = UsageSnapshot::new(state.settled.clone(), state.in_flight);
        snapshot.budget = self.0.budget.snapshot();
        Ok(snapshot)
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
