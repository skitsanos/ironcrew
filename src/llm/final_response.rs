//! Text required to finish a task or conversation, not intermediate tool calls.

use crate::utils::error::{IronCrewError, Result};

/// Reject absent/blank finals without treating reasoning as an answer or
/// trimming meaningful output. Call only after completing any tool-call rounds.
pub(crate) fn require_final_content(content: Option<String>) -> Result<String> {
    content
        .filter(|text| !text.trim().is_empty())
        .ok_or(IronCrewError::EmptyProviderResponse)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blank_finals_cannot_trigger_whole_task_retries() {
        for content in [None, Some(""), Some(" \n\t"), Some("\u{2003}\u{a0}")] {
            let error = require_final_content(content.map(str::to_owned)).unwrap_err();
            assert!(matches!(error, IronCrewError::EmptyProviderResponse));
            assert!(!error.allows_task_retry());
        }
    }

    #[test]
    fn nonblank_output_is_preserved_exactly() {
        let text = " \n  answer\u{2003}";
        assert_eq!(require_final_content(Some(text.into())).unwrap(), text);
    }

    #[test]
    fn other_provider_errors_keep_existing_retry_behavior() {
        assert!(IronCrewError::Provider("temporary failure".into()).allows_task_retry());
    }
}
