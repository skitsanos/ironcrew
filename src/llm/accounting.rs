//! Checked receipt capture at the HTTP dispatch boundary, independent of output.
use serde_json::Value;

use crate::usage::{ProviderUsage, StreamUsage, UsageAttempt, UsageReceipt, UsageTracker};
use crate::utils::error::{IronCrewError, Result};

pub(crate) struct ProviderAttempt {
    state: StreamUsage,
    attempt: Option<UsageAttempt>,
    final_receipt: bool,
    metrics_family: crate::metrics::ProviderFamily,
    metrics_recorded: bool,
    reservation: Option<crate::usage::budget::Reservation>,
}

impl ProviderAttempt {
    /// Call after local validation/rate admission, immediately before sending.
    pub(crate) fn start(tracker: Option<&UsageTracker>, provider: ProviderUsage) -> Result<Self> {
        Self::start_reserved(tracker, provider, None)
    }

    pub(crate) fn start_reserved(
        tracker: Option<&UsageTracker>,
        provider: ProviderUsage,
        reservation: Option<crate::usage::budget::Reservation>,
    ) -> Result<Self> {
        if reservation.is_none() {
            let budget = match tracker {
                Some(tracker) => tracker.budget().clone(),
                None => crate::usage::budget::TokenBudget::from_environment()?,
            };
            budget.check()?;
            if budget.enabled() {
                return Err(budget
                    .block(crate::usage::budget::BudgetError::Unsupported)
                    .into());
            }
        }
        Ok(Self {
            reservation,
            state: StreamUsage::new(provider),
            attempt: tracker
                .map(UsageTracker::start)
                .transpose()
                .map_err(accounting_error)?,
            final_receipt: false,
            metrics_family: match provider {
                ProviderUsage::OpenAiChat => crate::metrics::ProviderFamily::OpenAi,
                ProviderUsage::OpenAiResponses => crate::metrics::ProviderFamily::OpenAiResponses,
                ProviderUsage::Anthropic => crate::metrics::ProviderFamily::Anthropic,
            },
            metrics_recorded: false,
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
    pub(crate) fn finish(mut self) -> Result<UsageReceipt> {
        let receipt = self.state.snapshot(self.final_receipt);
        self.record_metrics(&receipt);
        if let Some(attempt) = self.attempt.take() {
            attempt.finish(receipt.clone()).map_err(accounting_error)?;
        }
        if let Some(reservation) = self.reservation.take() {
            reservation.finish(&receipt)?;
        }
        Ok(receipt)
    }

    fn record_metrics(&mut self, receipt: &UsageReceipt) {
        if !self.metrics_recorded {
            crate::metrics::record_provider_usage(self.metrics_family, receipt);
            self.metrics_recorded = true;
        }
    }
}

impl Drop for ProviderAttempt {
    fn drop(&mut self) {
        let receipt = self.state.snapshot(self.final_receipt);
        self.record_metrics(&receipt);
        if let Some(reservation) = self.reservation.take() {
            let _ = reservation.finish(&receipt);
        }
    }
}

fn accounting_error(error: crate::usage::UsageOverflow) -> IronCrewError {
    IronCrewError::Provider(error.to_string())
}
