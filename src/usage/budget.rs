//! Atomic admission for one process-local run, independent of receipt totals.
mod snapshot;
use super::{UsageCoverage, UsageReceipt};
pub use snapshot::{BudgetSnapshot, BudgetState};
use std::sync::{Arc, Mutex};

pub const MAX_RUN_TOKENS: u64 = 1_000_000_000;
pub const DEFAULT_BUDGET_OUTPUT_TOKENS: u64 = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum BudgetError {
    #[error(
        "IRONCREW_MAX_RUN_TOKENS must be an integer between 1 and 1000000000; unset disables budgeting"
    )]
    Configuration,
    #[error("run token budget exhausted; no further provider requests admitted")]
    Exhausted,
    #[error("run token budget cannot safely bound this provider/request")]
    Unsupported,
    #[error("provider usage exceeded its token reservation; run token budget is blocked")]
    BoundViolated,
    #[error("input token counting failed; run token budget is blocked")]
    CountingFailed,
}

#[derive(Debug, Clone, Default)]
pub struct TokenBudget(Option<Arc<Mutex<BudgetSnapshot>>>);

impl TokenBudget {
    pub fn from_environment() -> Result<Self, BudgetError> {
        match std::env::var("IRONCREW_MAX_RUN_TOKENS") {
            Ok(value) => Self::from_raw(Some(&value)),
            Err(std::env::VarError::NotPresent) => Ok(Self::default()),
            Err(_) => Err(BudgetError::Configuration),
        }
    }

    pub fn from_raw(raw: Option<&str>) -> Result<Self, BudgetError> {
        let Some(raw) = raw else {
            return Ok(Self::default());
        };
        if raw.is_empty() || !raw.bytes().all(|b| b.is_ascii_digit()) {
            return Err(BudgetError::Configuration);
        }
        Self::new(raw.parse().map_err(|_| BudgetError::Configuration)?)
    }

    pub fn new(limit: u64) -> Result<Self, BudgetError> {
        if !(1..=MAX_RUN_TOKENS).contains(&limit) {
            return Err(BudgetError::Configuration);
        }
        Ok(Self(Some(Arc::new(Mutex::new(BudgetSnapshot {
            state: BudgetState::Active,
            limit: Some(limit),
            ..Default::default()
        })))))
    }

    pub fn enabled(&self) -> bool {
        self.0.is_some()
    }

    pub fn snapshot(&self) -> BudgetSnapshot {
        self.0
            .as_ref()
            .map_or_else(BudgetSnapshot::default, |state| {
                state.lock().expect("token budget lock poisoned").clone()
            })
    }

    pub fn check(&self) -> Result<(), BudgetError> {
        self.snapshot().state.check()
    }

    /// Failure is sticky even if a concurrent request later releases capacity.
    pub fn block(&self, error: BudgetError) -> BudgetError {
        if let Some(state) = &self.0 {
            let mut state = state.lock().expect("token budget lock poisoned");
            if state.state == BudgetState::Active {
                state.state = BudgetState::from_error(error);
            }
        }
        error
    }

    /// Call immediately before generation dispatch, after counting the exact
    /// immutable input. No await may occur between admission and dispatch.
    pub fn reserve(&self, input: u64, output: u64) -> Result<Reservation, BudgetError> {
        let amount = input
            .checked_add(output)
            .ok_or_else(|| self.block(BudgetError::Exhausted))?;
        if output == 0 {
            return Err(self.block(BudgetError::Unsupported));
        }
        if let Some(state) = &self.0 {
            let mut state = state.lock().expect("token budget lock poisoned");
            state.state.check()?;
            let remaining = state.limit.expect("enabled budget") - state.charged - state.reserved;
            if amount > remaining {
                state.state = BudgetState::Exhausted;
                return Err(BudgetError::Exhausted);
            }
            state.reserved += amount;
            state.in_flight += 1;
        }
        Ok(Reservation {
            budget: self.clone(),
            input,
            output,
            settled: false,
        })
    }
}

/// Drop retains the whole bound: cancellation never proves zero provider cost.
#[derive(Debug)]
#[must_use]
pub struct Reservation {
    budget: TokenBudget,
    input: u64,
    output: u64,
    settled: bool,
}

impl Reservation {
    pub fn finish(mut self, receipt: &UsageReceipt) -> Result<(), BudgetError> {
        self.settle(receipt)
    }

    fn settle(&mut self, receipt: &UsageReceipt) -> Result<(), BudgetError> {
        if self.settled {
            return Ok(());
        }
        self.settled = true;
        let Some(state) = &self.budget.0 else {
            return Ok(());
        };
        let amount = self.input + self.output;
        let counts = receipt.counts();
        let violated = counts.prompt_tokens.is_some_and(|n| n > self.input)
            || counts.completion_tokens.is_some_and(|n| n > self.output)
            || counts.total_tokens.is_some_and(|n| n > amount);
        let known = receipt.coverage() == UsageCoverage::Complete && !violated;
        let charge = if known {
            counts.total_tokens.expect("complete receipt")
        } else {
            amount
        };
        let mut state = state.lock().expect("token budget lock poisoned");
        state.reserved -= amount;
        state.in_flight -= 1;
        state.charged += charge;
        if !known {
            state.retained += amount;
        }
        if violated {
            state.state = BudgetState::BoundViolated;
            return Err(BudgetError::BoundViolated);
        }
        // An exact final spend is allowed; the next admission fails if needed.
        Ok(())
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        let _ = self.settle(&UsageReceipt::default());
    }
}
