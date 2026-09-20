//! Chat Completions `reasoning_effort` handling, including the GPT-5.6 Luna
//! tool-compatibility rule.

use crate::utils::error::{IronCrewError, Result};

pub(super) fn is_luna(model: &str) -> bool {
    model == "gpt-5.6-luna" || model.starts_with("gpt-5.6-luna-")
}

/// An explicit per-agent effort is honored on Chat Completions except where the
/// API is known to reject it: Luna refuses function tools unless reasoning is
/// off. Fail before the request rather than silently overriding the author.
pub(super) fn validate_reasoning_effort(
    effort: Option<&str>,
    model: &str,
    has_tools: bool,
) -> Result<()> {
    match effort {
        Some(value) if has_tools && is_luna(model) && value != "none" => {
            Err(IronCrewError::Validation(format!(
                "reasoning_effort '{value}' is not supported together with tools on {model} \
                 via Chat Completions; use provider = \"openai-responses\" or set \
                 reasoning_effort = \"none\""
            )))
        }
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::validate_reasoning_effort;

    #[test]
    fn luna_with_tools_rejects_a_non_none_effort() {
        let error = validate_reasoning_effort(Some("high"), "gpt-5.6-luna", true)
            .unwrap_err()
            .to_string();
        assert!(error.contains("reasoning_effort"), "{error}");
        assert!(error.contains("openai-responses"), "{error}");
        assert!(validate_reasoning_effort(Some("none"), "gpt-5.6-luna", true).is_ok());
        assert!(validate_reasoning_effort(Some("high"), "gpt-5.6-luna", false).is_ok());
        assert!(validate_reasoning_effort(Some("high"), "gpt-5.4", true).is_ok());
        assert!(validate_reasoning_effort(None, "gpt-5.6-luna", true).is_ok());
    }
}
