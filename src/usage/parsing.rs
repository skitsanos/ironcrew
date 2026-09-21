use serde_json::Value;

use super::{UsageCounts, UsageReceipt};

/// The wire usage vocabulary, independent of model name and response content.
#[derive(Debug, Clone, Copy)]
pub enum ProviderUsage {
    OpenAiChat,
    OpenAiResponses,
    Anthropic,
}

impl ProviderUsage {
    /// Parse the `usage` object, not the whole response. Missing or malformed
    /// counts never become known zero. No values are narrowed to 32 bits.
    pub fn parse(self, usage: Option<&Value>, final_receipt: bool) -> UsageReceipt {
        let mut state = StreamUsage::new(self);
        if let Some(usage) = usage {
            state.update(usage);
        }
        state.finish(final_receipt)
    }

    fn paths(self) -> [&'static [&'static str]; 6] {
        match self {
            Self::OpenAiChat => [
                &["prompt_tokens"],
                &["completion_tokens"],
                &["total_tokens"],
                &["prompt_tokens_details", "cached_tokens"],
                &["prompt_tokens_details", "cache_write_tokens"],
                &["completion_tokens_details", "reasoning_tokens"],
            ],
            Self::OpenAiResponses => [
                &["input_tokens"],
                &["output_tokens"],
                &["total_tokens"],
                &["input_tokens_details", "cached_tokens"],
                &["input_tokens_details", "cache_write_tokens"],
                &["output_tokens_details", "reasoning_tokens"],
            ],
            Self::Anthropic => [
                &["input_tokens"],
                &["output_tokens"],
                &["total_tokens"],
                &["cache_read_input_tokens"],
                &["cache_creation_input_tokens"],
                &["output_tokens_details", "thinking_tokens"],
            ],
        }
    }
}

/// Constant-space cumulative usage assembly for ONE request. Updates replace
/// counts; they are never added together. Clone a snapshot on interruption.
/// The transport, not this parser, must recognize a final usage receipt.
#[derive(Debug)]
pub struct StreamUsage {
    provider: ProviderUsage,
    fields: [Option<u64>; 6],
    tainted: bool,
}

impl StreamUsage {
    pub fn new(provider: ProviderUsage) -> Self {
        Self {
            provider,
            fields: [None; 6],
            tainted: false,
        }
    }

    /// Only six scalar fields are retained. Unrecognized provider metadata
    /// cannot expand retained memory. Null usage chunks carry no new receipt.
    pub fn update(&mut self, usage: &Value) {
        if usage.is_null() {
            return;
        }
        if !usage.is_object() {
            self.tainted = true;
            return;
        }
        for (index, path) in self.provider.paths().into_iter().enumerate() {
            // Anthropic has no total_tokens field; calculate its normalized
            // input + output below, including separate cache input categories.
            if index == 2 && matches!(self.provider, ProviderUsage::Anthropic) {
                continue;
            }
            let Some(value) = field(usage, path) else {
                continue;
            };
            match value {
                Ok(count) if self.fields[index].is_none_or(|prior| count >= prior) => {
                    self.fields[index] = Some(count);
                }
                // A malformed/regressing update cannot erase an earlier
                // known subtotal or be presented as a complete final receipt.
                _ => self.tainted = true,
            }
        }
    }

    /// `true` means final accounting was received, not that generated content
    /// passed validation. `false` preserves partial counts on failure/cancel.
    pub fn snapshot(&self, final_receipt: bool) -> UsageReceipt {
        let [input, output, mut total, cached, cache_write, reasoning] = self.fields;
        let mut input = input;
        let mut valid = !self.tainted;
        if matches!(self.provider, ProviderUsage::Anthropic) {
            valid &= [input, output, cached, cache_write]
                .iter()
                .all(Option::is_some);
            // Preserve known categories as a lower bound when another
            // category is absent. The receipt cannot then be complete.
            let input_fields = [input, cached, cache_write];
            input = sum_known(input_fields);
            let input_overflow = input.is_none() && input_fields.iter().any(Option::is_some);
            total = if input_overflow {
                None
            } else {
                sum_known([input, output])
            };
            valid &= input.is_some() && total.is_some();
        }
        UsageReceipt::from_counts(
            UsageCounts {
                prompt_tokens: input,
                completion_tokens: output,
                total_tokens: total,
                cached_tokens: cached,
                cache_write_tokens: cache_write,
                reasoning_tokens: reasoning,
            },
            final_receipt && valid,
        )
    }

    pub fn finish(self, final_receipt: bool) -> UsageReceipt {
        self.snapshot(final_receipt)
    }
}

// Absent fields do not overwrite cumulative state. A present malformed
// intermediate container is returned as invalid rather than treated as absent.
fn field(mut value: &Value, path: &[&str]) -> Option<Result<u64, ()>> {
    for key in path {
        if !value.is_object() {
            return Some(Err(()));
        }
        value = value.get(*key)?;
    }
    Some(value.as_u64().ok_or(()))
}

fn sum_known<const N: usize>(values: [Option<u64>; N]) -> Option<u64> {
    let mut known = None;
    for value in values.into_iter().flatten() {
        known = Some(known.unwrap_or(0_u64).checked_add(value)?);
    }
    known
}
