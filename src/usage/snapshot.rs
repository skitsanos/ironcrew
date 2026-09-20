use super::{UsageAggregate, UsageCoverage};
use serde::{Deserialize, Serialize};

/// An inclusive scope's settled receipts and outstanding attempts. Decimal
/// strings in the wire format preserve the entire unsigned 64-bit range.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "SnapshotWire")]
pub struct UsageSnapshot {
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
    pub(crate) fn new(settled: UsageAggregate, in_flight: u64) -> Self {
        let coverage = if in_flight == 0 {
            settled.coverage()
        } else if settled.requests() == 0 || settled.coverage() == UsageCoverage::Unavailable {
            UsageCoverage::Unavailable
        } else {
            UsageCoverage::Partial
        };
        Self {
            settled,
            in_flight,
            coverage,
        }
    }

    /// No trustworthy execution checkpoint exists (for example after owner
    /// death). Do not manufacture a zero-cost or zero-request receipt.
    pub fn unavailable() -> Self {
        Self::new(UsageAggregate::unavailable(), 0)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SnapshotWire {
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
        let value = Self::new(wire.settled, wire.in_flight);
        if value.coverage != wire.coverage {
            return Err("inconsistent usage snapshot coverage");
        }
        Ok(value)
    }
}
