use crate::engine::task::TaskTokenUsage;
use crate::llm::provider::TokenUsage;

/// Aggregates provider usage only while every collaborative call supplies a
/// coherent, non-zero receipt. Partial usage must remain unknown rather than
/// looking like complete accounting to callers.
#[derive(Default)]
pub(super) struct UsageAccumulator {
    total: TaskTokenUsage,
    complete: bool,
    observed: bool,
}

impl UsageAccumulator {
    pub(super) fn observe(&mut self, usage: Option<&TokenUsage>) {
        let Some(usage) = usage else {
            self.complete = false;
            self.observed = true;
            return;
        };
        let coherent = usage.prompt_tokens > 0
            && usage.completion_tokens > 0
            && usage.total_tokens > 0
            && usage.cached_tokens <= usage.prompt_tokens
            && usage.total_tokens == usage.prompt_tokens.saturating_add(usage.completion_tokens);
        if !coherent {
            self.complete = false;
            self.observed = true;
            return;
        }
        let next = (
            self.total.prompt_tokens.checked_add(usage.prompt_tokens),
            self.total
                .completion_tokens
                .checked_add(usage.completion_tokens),
            self.total.total_tokens.checked_add(usage.total_tokens),
            self.total.cached_tokens.checked_add(usage.cached_tokens),
        );
        let (Some(prompt), Some(completion), Some(total), Some(cached)) = next else {
            self.complete = false;
            self.observed = true;
            return;
        };
        self.total = TaskTokenUsage {
            prompt_tokens: prompt,
            completion_tokens: completion,
            total_tokens: total,
            cached_tokens: cached,
        };
        if !self.observed {
            self.complete = true;
        }
        self.observed = true;
    }

    pub(super) fn finish(self) -> Option<TaskTokenUsage> {
        (self.observed && self.complete).then_some(self.total)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(prompt: u32, completion: u32) -> TokenUsage {
        TokenUsage {
            prompt_tokens: prompt,
            completion_tokens: completion,
            total_tokens: prompt + completion,
            cached_tokens: 0,
        }
    }

    #[test]
    fn mixed_present_and_missing_usage_remains_unknown() {
        let mut accumulator = UsageAccumulator::default();
        accumulator.observe(Some(&usage(10, 2)));
        accumulator.observe(None);
        accumulator.observe(Some(&usage(20, 3)));

        assert!(accumulator.finish().is_none());
    }

    #[test]
    fn one_zero_completion_receipt_makes_the_aggregate_unknown() {
        let mut accumulator = UsageAccumulator::default();
        accumulator.observe(Some(&usage(10, 2)));
        accumulator.observe(Some(&usage(20, 0)));
        accumulator.observe(Some(&usage(30, 3)));

        assert!(accumulator.finish().is_none());
    }

    #[test]
    fn complete_coherent_usage_is_summed() {
        let mut accumulator = UsageAccumulator::default();
        accumulator.observe(Some(&usage(10, 2)));
        accumulator.observe(Some(&usage(20, 3)));

        let total = accumulator.finish().expect("complete usage");
        assert_eq!(total.prompt_tokens, 30);
        assert_eq!(total.completion_tokens, 5);
        assert_eq!(total.total_tokens, 35);
    }
}
