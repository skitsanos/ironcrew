//! Request-shape guards applied before any Anthropic API call.

use crate::llm::provider::ChatRequest;
use crate::utils::error::{IronCrewError, Result};

/// The Anthropic Messages API has no `reasoning_effort`; its analogue is the
/// crew-level `thinking_budget`. Refuse rather than silently drop the option.
pub(super) fn reject_reasoning_effort(request: &ChatRequest) -> Result<()> {
    match request.reasoning_effort.as_deref() {
        Some(effort) => Err(IronCrewError::Validation(format!(
            "reasoning_effort '{effort}' is not supported by the anthropic provider; \
             use the crew-level thinking_budget instead"
        ))),
        None => Ok(()),
    }
}
