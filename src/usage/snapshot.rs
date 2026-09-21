use super::{UsageAggregate, UsageCoverage};
use serde::{Deserialize, Serialize};

/// An inclusive scope's settled receipts and outstanding attempts. Decimal
/// strings in the wire format preserve the entire unsigned 64-bit range.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "SnapshotWire")]
pub struct UsageSnapshot {
    pub budget: super::budget::BudgetSnapshot,
    pub settled: UsageAggregate,
    #[serde(with = "super::wire")]
    pub in_flight: u64,
    pub coverage: UsageCoverage,
}

impl Default for UsageSnapshot {
    fn default() -> Self {
        Self::new(UsageAggregate::default(), 0)
    }
}

impl UsageSnapshot {
    pub fn validate(&self) -> Result<(), &'static str> {
        self.budget.validate()?;
        self.settled.validate()?;
        self.settled
            .requests()
            .checked_add(self.in_flight)
            .ok_or("usage request count overflow")?;
        if Self::new(self.settled.clone(), self.in_flight).coverage != self.coverage {
            return Err("inconsistent usage snapshot coverage");
        }
        Ok(())
    }

    /// A settled snapshot of exactly one checked provider receipt.
    pub fn from_receipt(receipt: super::UsageReceipt) -> Self {
        let mut settled = UsageAggregate::default();
        settled
            .add(&receipt)
            .expect("one checked receipt fits an empty aggregate");
        Self::new(settled, 0)
    }

    pub(crate) fn new(settled: UsageAggregate, in_flight: u64) -> Self {
        let coverage = if in_flight == 0 {
            settled.coverage()
        } else if settled.requests() == 0 || settled.coverage() == UsageCoverage::Unavailable {
            UsageCoverage::Unavailable
        } else {
            UsageCoverage::Partial
        };
        Self {
            budget: Default::default(),
            settled,
            in_flight,
            coverage,
        }
    }

    /// No trustworthy execution checkpoint exists (for example after owner
    /// death). Do not manufacture a zero-cost or zero-request receipt.
    pub fn unavailable() -> Self {
        let mut snapshot = Self::new(UsageAggregate::unavailable(), 0);
        snapshot.budget.state = super::budget::BudgetState::Unavailable;
        snapshot
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SnapshotWire {
    budget: super::budget::BudgetSnapshot,
    settled: UsageAggregate,
    #[serde(with = "super::wire")]
    in_flight: u64,
    coverage: UsageCoverage,
}

impl TryFrom<SnapshotWire> for UsageSnapshot {
    type Error = &'static str;
    fn try_from(wire: SnapshotWire) -> Result<Self, Self::Error> {
        wire.settled
            .requests()
            .checked_add(wire.in_flight)
            .ok_or("usage request count overflow")?;
        let mut value = Self::new(wire.settled, wire.in_flight);
        wire.budget.validate()?;
        value.budget = wire.budget;
        if value.coverage != wire.coverage {
            return Err("inconsistent usage snapshot coverage");
        }
        Ok(value)
    }
}
