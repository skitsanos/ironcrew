use super::*;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CountWire {
    #[serde(with = "crate::usage::wire::optional")]
    known: Option<u64>,
    complete: bool,
}

impl TryFrom<CountWire> for CountTotal {
    type Error = &'static str;
    fn try_from(wire: CountWire) -> Result<Self, Self::Error> {
        if wire.complete && wire.known.is_none() {
            return Err("complete usage requires a known count");
        }
        Ok(Self {
            known: wire.known,
            complete: wire.complete,
        })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct AggregateWire {
    #[serde(with = "crate::usage::wire")]
    requests: u64,
    coverage: UsageCoverage,
    prompt_tokens: CountTotal,
    completion_tokens: CountTotal,
    total_tokens: CountTotal,
    cached_tokens: CountTotal,
    cache_write_tokens: CountTotal,
    reasoning_tokens: CountTotal,
}

impl TryFrom<AggregateWire> for UsageAggregate {
    type Error = &'static str;
    fn try_from(w: AggregateWire) -> Result<Self, Self::Error> {
        let value = Self {
            requests: w.requests,
            coverage: w.coverage,
            prompt_tokens: w.prompt_tokens,
            completion_tokens: w.completion_tokens,
            total_tokens: w.total_tokens,
            cached_tokens: w.cached_tokens,
            cache_write_tokens: w.cache_write_tokens,
            reasoning_tokens: w.reasoning_tokens,
        };
        value.validate()?;
        Ok(value)
    }
}

impl UsageAggregate {
    pub(crate) fn validate(&self) -> Result<(), &'static str> {
        let primary = [
            &self.prompt_tokens,
            &self.completion_tokens,
            &self.total_tokens,
        ];
        let all = [
            &self.prompt_tokens,
            &self.completion_tokens,
            &self.total_tokens,
            &self.cached_tokens,
            &self.cache_write_tokens,
            &self.reasoning_tokens,
        ];
        if self.requests == 0 {
            return if *self == Self::default() || *self == Self::unavailable() {
                Ok(())
            } else {
                Err("zero observed requests cannot carry token receipts")
            };
        }
        let expected = if primary.iter().all(|count| count.complete) {
            UsageCoverage::Complete
        } else if all.iter().any(|count| count.known.is_some()) {
            UsageCoverage::Partial
        } else {
            UsageCoverage::Unavailable
        };
        if expected != self.coverage {
            return Err("inconsistent aggregate usage coverage");
        }
        if primary.iter().all(|count| count.complete)
            && self
                .prompt_tokens
                .known
                .unwrap()
                .checked_add(self.completion_tokens.known.unwrap())
                != self.total_tokens.known
        {
            return Err("inconsistent aggregate usage total");
        }
        if self.total_tokens.complete
            && let (Some(input), Some(output), Some(total)) = (
                self.prompt_tokens.known,
                self.completion_tokens.known,
                self.total_tokens.known,
            )
            && input.checked_add(output).is_none_or(|sum| sum > total)
        {
            return Err("aggregate primary subtotals exceed complete total");
        }
        for (detail, parent) in [
            (&self.prompt_tokens, &self.total_tokens),
            (&self.completion_tokens, &self.total_tokens),
            (&self.cached_tokens, &self.prompt_tokens),
            (&self.cache_write_tokens, &self.prompt_tokens),
            (&self.reasoning_tokens, &self.completion_tokens),
        ] {
            if parent.complete
                && matches!((detail.known, parent.known), (Some(a), Some(b)) if a > b)
            {
                return Err("aggregate usage subset exceeds complete parent");
            }
        }
        if self.prompt_tokens.complete
            && let (Some(read), Some(write), Some(input)) = (
                self.cached_tokens.known,
                self.cache_write_tokens.known,
                self.prompt_tokens.known,
            )
            && read.checked_add(write).is_none_or(|sum| sum > input)
        {
            return Err("aggregate cache categories exceed complete input");
        }
        Ok(())
    }
}
