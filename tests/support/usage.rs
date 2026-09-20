use ironcrew::usage::{UsageCounts, UsageReceipt, UsageSnapshot};

/// Synthetic checked receipt, not an adapter for legacy stored counters.
pub fn fixture_usage(total: u64, cached: u64) -> UsageSnapshot {
    UsageSnapshot::from_receipt(UsageReceipt::from_counts(
        UsageCounts {
            prompt_tokens: Some(total),
            completion_tokens: Some(0),
            total_tokens: Some(total),
            cached_tokens: Some(cached),
            cache_write_tokens: Some(0),
            reasoning_tokens: Some(0),
        },
        true,
    ))
}
