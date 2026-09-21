use super::{BudgetError, MAX_RUN_TOKENS};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BudgetState {
    #[default]
    Disabled,
    Unavailable,
    Active,
    Exhausted,
    Unsupported,
    BoundViolated,
    CountingFailed,
}

impl BudgetState {
    pub fn check(self) -> Result<(), BudgetError> {
        match self {
            Self::Disabled | Self::Active => Ok(()),
            Self::Exhausted => Err(BudgetError::Exhausted),
            Self::Unsupported => Err(BudgetError::Unsupported),
            Self::Unavailable => Err(BudgetError::Unsupported),
            Self::BoundViolated => Err(BudgetError::BoundViolated),
            Self::CountingFailed => Err(BudgetError::CountingFailed),
        }
    }
    pub(super) fn from_error(error: BudgetError) -> Self {
        match error {
            BudgetError::Exhausted => Self::Exhausted,
            BudgetError::BoundViolated => Self::BoundViolated,
            BudgetError::CountingFailed => Self::CountingFailed,
            BudgetError::Unsupported | BudgetError::Configuration => Self::Unsupported,
        }
    }
}

/// Capacity accounting, not billing. `retained` is the subset of `charged`
/// kept conservatively because a complete bounded receipt was unavailable.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BudgetSnapshot {
    pub state: BudgetState,
    #[serde(with = "crate::usage::wire::optional")]
    pub limit: Option<u64>,
    #[serde(with = "crate::usage::wire")]
    pub charged: u64,
    #[serde(with = "crate::usage::wire")]
    pub retained: u64,
    #[serde(with = "crate::usage::wire")]
    pub reserved: u64,
    #[serde(with = "crate::usage::wire")]
    pub in_flight: u64,
}

impl BudgetSnapshot {
    pub fn validate(&self) -> Result<(), &'static str> {
        if matches!(self.state, BudgetState::Disabled | BudgetState::Unavailable) {
            return if self
                == &(Self {
                    state: self.state,
                    ..Self::default()
                }) {
                Ok(())
            } else {
                Err("invalid disabled token budget")
            };
        }
        let limit = self
            .limit
            .filter(|n| (1..=MAX_RUN_TOKENS).contains(n))
            .ok_or("invalid token budget limit")?;
        if self
            .charged
            .checked_add(self.reserved)
            .is_none_or(|n| n > limit)
            || self.retained > self.charged
            || (self.in_flight == 0) != (self.reserved == 0)
            || self.in_flight > self.reserved
        {
            return Err("invalid token budget counters");
        }
        Ok(())
    }
}
