//! Checked usage-receipt foundation for IC-046.
//!
//! Built-in HTTP providers can capture into an explicit request scope. Runtime
//! task results and durable stores have not yet migrated to these types.
//! Counts describe provider receipts, not invoices or inferred text lengths.
mod aggregate;
mod parsing;
mod tracker;

pub use aggregate::{CountTotal, UsageAggregate, UsageOverflow};
pub use parsing::{ProviderUsage, StreamUsage};
pub use tracker::{UsageAttempt, UsageSnapshot, UsageTracker};

use serde::{Deserialize, Serialize};

/// Coverage of primary input, output and total counts, not optional details.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageCoverage {
    Complete,
    Partial,
    Unavailable,
}

/// Normalized counts. `None` means unknown, not zero. Cache and reasoning
/// counts are subsets of input and output respectively, never extra totals.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UsageCounts {
    pub prompt_tokens: Option<u64>,
    pub completion_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
    pub cached_tokens: Option<u64>,
    pub cache_write_tokens: Option<u64>,
    pub reasoning_tokens: Option<u64>,
}

impl UsageCounts {
    fn normalize(&mut self) -> bool {
        let mut valid = true;
        if let Some(total) = self.total_tokens
            && [self.prompt_tokens, self.completion_tokens]
                .into_iter()
                .flatten()
                .any(|count| count > total)
        {
            self.total_tokens = None;
            valid = false;
        }
        if let (Some(input), Some(output), Some(total)) = (
            self.prompt_tokens,
            self.completion_tokens,
            self.total_tokens,
        ) && input.checked_add(output) != Some(total)
        {
            self.total_tokens = None;
            valid = false;
        }
        for (detail, parent) in [
            (&mut self.cached_tokens, self.prompt_tokens),
            (&mut self.cache_write_tokens, self.prompt_tokens),
            (&mut self.reasoning_tokens, self.completion_tokens),
        ] {
            if matches!((*detail, parent), (Some(value), Some(limit)) if value > limit) {
                *detail = None;
                valid = false;
            }
        }
        if let (Some(read), Some(write), Some(input)) = (
            self.cached_tokens,
            self.cache_write_tokens,
            self.prompt_tokens,
        ) && read.checked_add(write).is_none_or(|sum| sum > input)
        {
            self.cached_tokens = None;
            self.cache_write_tokens = None;
            valid = false;
        }
        valid
    }

    fn any_known(&self) -> bool {
        self.prompt_tokens.is_some()
            || self.completion_tokens.is_some()
            || self.total_tokens.is_some()
            || self.cached_tokens.is_some()
            || self.cache_write_tokens.is_some()
            || self.reasoning_tokens.is_some()
    }
}

/// One provider attempt's latest cumulative receipt. A failed output can have
/// a complete receipt; a successful output can have unavailable usage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "ReceiptWire")]
pub struct UsageReceipt {
    counts: UsageCounts,
    final_receipt: bool,
    coverage: UsageCoverage,
}

impl Default for UsageReceipt {
    fn default() -> Self {
        Self::from_counts(UsageCounts::default(), false)
    }
}

impl UsageReceipt {
    /// Invalid relationships discard the contradictory count and prevent a
    /// complete claim, while preserving independently known counts.
    pub fn from_counts(mut counts: UsageCounts, final_receipt: bool) -> Self {
        let final_receipt = counts.normalize() && final_receipt;
        let coverage = if final_receipt
            && counts.prompt_tokens.is_some()
            && counts.completion_tokens.is_some()
            && counts.total_tokens.is_some()
        {
            UsageCoverage::Complete
        } else if counts.any_known() {
            UsageCoverage::Partial
        } else {
            UsageCoverage::Unavailable
        };
        Self {
            counts,
            final_receipt,
            coverage,
        }
    }

    pub fn counts(&self) -> &UsageCounts {
        &self.counts
    }
    pub fn coverage(&self) -> UsageCoverage {
        self.coverage
    }
    pub fn is_final(&self) -> bool {
        self.final_receipt
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReceiptWire {
    counts: UsageCounts,
    final_receipt: bool,
    coverage: UsageCoverage,
}

impl TryFrom<ReceiptWire> for UsageReceipt {
    type Error = &'static str;

    fn try_from(wire: ReceiptWire) -> Result<Self, Self::Error> {
        let receipt = Self::from_counts(wire.counts.clone(), wire.final_receipt);
        if receipt.counts != wire.counts
            || receipt.final_receipt != wire.final_receipt
            || receipt.coverage != wire.coverage
        {
            return Err("inconsistent usage receipt");
        }
        Ok(receipt)
    }
}
