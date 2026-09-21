//! Fixed-cardinality lower-bound token counters with explicit missing coverage.
use super::{ProviderFamily, TokenKind, histogram::saturating_add, state::Metrics};
use crate::usage::{UsageCoverage, UsageReceipt};
use std::fmt::Write;
use std::sync::atomic::AtomicU64;

const COVERAGES: [&str; 3] = ["complete", "partial", "unavailable"];

pub(super) struct UsageMetrics {
    tokens: [[AtomicU64; TokenKind::COUNT]; ProviderFamily::COUNT],
    incomplete: [[AtomicU64; TokenKind::COUNT]; ProviderFamily::COUNT],
    receipts: [[AtomicU64; 3]; ProviderFamily::COUNT],
}

impl Default for UsageMetrics {
    fn default() -> Self {
        Self {
            tokens: std::array::from_fn(|_| std::array::from_fn(|_| AtomicU64::new(0))),
            incomplete: std::array::from_fn(|_| std::array::from_fn(|_| AtomicU64::new(0))),
            receipts: std::array::from_fn(|_| std::array::from_fn(|_| AtomicU64::new(0))),
        }
    }
}

impl UsageMetrics {
    pub(super) fn record(&self, family: ProviderFamily, receipt: &UsageReceipt) {
        let family = family.index();
        let coverage = match receipt.coverage() {
            UsageCoverage::Complete => 0,
            UsageCoverage::Partial => 1,
            UsageCoverage::Unavailable => 2,
        };
        saturating_add(&self.receipts[family][coverage], 1);
        let counts = receipt.counts();
        for (kind, value) in [
            (TokenKind::Prompt, counts.prompt_tokens),
            (TokenKind::Completion, counts.completion_tokens),
            (TokenKind::Cached, counts.cached_tokens),
            (TokenKind::Total, counts.total_tokens),
            (TokenKind::CacheWrite, counts.cache_write_tokens),
            (TokenKind::Reasoning, counts.reasoning_tokens),
        ] {
            if let Some(value) = value {
                saturating_add(&self.tokens[family][kind.index()], value);
            }
            if value.is_none() || !receipt.is_final() {
                saturating_add(&self.incomplete[family][kind.index()], 1);
            }
        }
    }

    pub(super) fn append(&self, body: &mut String) {
        for metric in [
            "ironcrew_provider_tokens_total",
            "ironcrew_provider_usage_incomplete_fields_total",
            "ironcrew_provider_usage_receipts_total",
        ] {
            writeln!(body, "# TYPE {metric} counter").unwrap();
        }
        for &family in ProviderFamily::ALL {
            for &kind in TokenKind::ALL {
                let labels = format!(
                    "provider=\"{}\",type=\"{}\"",
                    family.as_str(),
                    kind.as_str()
                );
                let known = Metrics::counter(&self.tokens[family.index()][kind.index()]);
                let incomplete = Metrics::counter(&self.incomplete[family.index()][kind.index()]);
                writeln!(body, "ironcrew_provider_tokens_total{{{labels}}} {known}").unwrap();
                writeln!(
                    body,
                    "ironcrew_provider_usage_incomplete_fields_total{{{labels}}} {incomplete}"
                )
                .unwrap();
            }
            for (index, coverage) in COVERAGES.into_iter().enumerate() {
                let count = Metrics::counter(&self.receipts[family.index()][index]);
                writeln!(body, "ironcrew_provider_usage_receipts_total{{provider=\"{}\",coverage=\"{coverage}\"}} {count}", family.as_str()).unwrap();
            }
        }
    }
}
