use serde::Serialize;

use super::{UsageCoverage, UsageReceipt};

/// Arithmetic failure. The aggregate remains unchanged; callers must not
/// discard this error and continue advertising complete accounting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("usage accounting exceeds the supported 64-bit range")]
pub struct UsageOverflow;

/// Known subtotal plus its own coverage. Optional detail coverage is separate
/// from primary usage coverage. An unavailable subtotal serializes as null.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CountTotal {
    known: Option<u64>,
    complete: bool,
}

impl Default for CountTotal {
    fn default() -> Self {
        Self {
            known: Some(0),
            complete: true,
        }
    }
}

impl CountTotal {
    pub fn known(&self) -> Option<u64> {
        self.known
    }
    pub fn complete(&self) -> bool {
        self.complete
    }

    fn from_receipt(value: Option<u64>, final_receipt: bool) -> Self {
        Self {
            known: value,
            complete: value.is_some() && final_receipt,
        }
    }

    fn merge(&self, other: &Self) -> Result<Self, UsageOverflow> {
        let known = match (self.known, other.known) {
            (Some(a), Some(b)) => Some(a.checked_add(b).ok_or(UsageOverflow)?),
            (Some(value), None) | (None, Some(value)) => Some(value),
            (None, None) => None,
        };
        Ok(Self {
            known,
            complete: self.complete && other.complete,
        })
    }
}

/// Bounded aggregate of disjoint provider attempts. Add ONE final snapshot per
/// attempt, including failed/cancelled attempts. Merge only disjoint scopes;
/// merging a child and its inclusive parent would count the child twice.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UsageAggregate {
    requests: u64,
    coverage: UsageCoverage,
    prompt_tokens: CountTotal,
    completion_tokens: CountTotal,
    total_tokens: CountTotal,
    cached_tokens: CountTotal,
    cache_write_tokens: CountTotal,
    reasoning_tokens: CountTotal,
}

impl Default for UsageAggregate {
    fn default() -> Self {
        Self {
            requests: 0,
            coverage: UsageCoverage::Complete,
            prompt_tokens: CountTotal::default(),
            completion_tokens: CountTotal::default(),
            total_tokens: CountTotal::default(),
            cached_tokens: CountTotal::default(),
            cache_write_tokens: CountTotal::default(),
            reasoning_tokens: CountTotal::default(),
        }
    }
}

impl UsageAggregate {
    pub fn requests(&self) -> u64 {
        self.requests
    }
    pub fn coverage(&self) -> UsageCoverage {
        self.coverage
    }
    pub fn prompt_tokens(&self) -> &CountTotal {
        &self.prompt_tokens
    }
    pub fn completion_tokens(&self) -> &CountTotal {
        &self.completion_tokens
    }
    pub fn total_tokens(&self) -> &CountTotal {
        &self.total_tokens
    }
    pub fn cached_tokens(&self) -> &CountTotal {
        &self.cached_tokens
    }
    pub fn cache_write_tokens(&self) -> &CountTotal {
        &self.cache_write_tokens
    }
    pub fn reasoning_tokens(&self) -> &CountTotal {
        &self.reasoning_tokens
    }

    pub fn add(&mut self, receipt: &UsageReceipt) -> Result<(), UsageOverflow> {
        let counts = receipt.counts();
        let tally = |value| CountTotal::from_receipt(value, receipt.is_final());
        self.merge(&Self {
            requests: 1,
            coverage: receipt.coverage(),
            prompt_tokens: tally(counts.prompt_tokens),
            completion_tokens: tally(counts.completion_tokens),
            total_tokens: tally(counts.total_tokens),
            cached_tokens: tally(counts.cached_tokens),
            cache_write_tokens: tally(counts.cache_write_tokens),
            reasoning_tokens: tally(counts.reasoning_tokens),
        })
    }

    /// Checked and atomic: neither a token overflow nor a request-count
    /// overflow can wrap, saturate, or partially mutate this aggregate.
    pub fn merge(&mut self, other: &Self) -> Result<(), UsageOverflow> {
        if other.requests == 0 {
            return Ok(());
        }
        if self.requests == 0 {
            *self = other.clone();
            return Ok(());
        }
        let coverage = match (self.coverage, other.coverage) {
            (UsageCoverage::Complete, UsageCoverage::Complete) => UsageCoverage::Complete,
            (UsageCoverage::Unavailable, UsageCoverage::Unavailable) => UsageCoverage::Unavailable,
            _ => UsageCoverage::Partial,
        };
        let merged = Self {
            requests: self
                .requests
                .checked_add(other.requests)
                .ok_or(UsageOverflow)?,
            coverage,
            prompt_tokens: self.prompt_tokens.merge(&other.prompt_tokens)?,
            completion_tokens: self.completion_tokens.merge(&other.completion_tokens)?,
            total_tokens: self.total_tokens.merge(&other.total_tokens)?,
            cached_tokens: self.cached_tokens.merge(&other.cached_tokens)?,
            cache_write_tokens: self.cache_write_tokens.merge(&other.cache_write_tokens)?,
            reasoning_tokens: self.reasoning_tokens.merge(&other.reasoning_tokens)?,
        };
        *self = merged;
        Ok(())
    }
}
