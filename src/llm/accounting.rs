//! Checked receipt capture at the HTTP dispatch boundary, independent of output.
use serde_json::Value;

use crate::usage::{ProviderUsage, StreamUsage, UsageAttempt, UsageTracker};
use crate::utils::error::{IronCrewError, Result};

pub(crate) struct ProviderAttempt {
    state: StreamUsage,
    attempt: Option<UsageAttempt>,
    final_receipt: bool,
}

impl ProviderAttempt {
    /// Call after local validation/rate admission, immediately before sending.
    pub(crate) fn start(tracker: Option<&UsageTracker>, provider: ProviderUsage) -> Result<Self> {
        Ok(Self {
            state: StreamUsage::new(provider),
            attempt: tracker
                .map(UsageTracker::start)
                .transpose()
                .map_err(accounting_error)?,
            final_receipt: false,
        })
    }

    /// Observe before any content processing or await that may fail/cancel.
    /// Final means a terminal accounting receipt, not a successful model reply.
    pub(crate) fn observe(&mut self, usage: Option<&Value>, final_receipt: bool) {
        if let Some(usage) = usage.filter(|value| !value.is_null()) {
            self.state.update(usage);
            // A later nonterminal update cannot inherit an earlier terminal
            // claim. Null/missing chunks carry no update and preserve it.
            self.final_receipt = final_receipt;
        } else if final_receipt {
            self.final_receipt = true;
        }
        if let Some(attempt) = &mut self.attempt {
            attempt.observe(self.state.snapshot(self.final_receipt));
        }
    }

    /// Success must surface settlement overflow; Drop handles failure/cancel.
    pub(crate) fn finish(mut self) -> Result<()> {
        if let Some(attempt) = self.attempt.take() {
            attempt
                .finish(self.state.snapshot(self.final_receipt))
                .map_err(accounting_error)?;
        }
        Ok(())
    }
}

fn accounting_error(error: crate::usage::UsageOverflow) -> IronCrewError {
    IronCrewError::Provider(error.to_string())
}
