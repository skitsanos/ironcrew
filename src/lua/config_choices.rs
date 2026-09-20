//! Closed value sets for `Crew.new` options that the provider APIs treat as
//! enums, validated at construction so a typo fails before the first request.

use mlua::Result as LuaResult;

use crate::utils::error::IronCrewError;

/// Responses API `reasoning.effort` values — the union across OpenAI model
/// families (`minimal` on older reasoning models; `xhigh`/`max` on the newest,
/// e.g. gpt-5.6-luna). Validation catches typos; the API arbitrates which
/// values a given model honours.
pub(crate) const REASONING_EFFORTS: &[&str] =
    &["none", "minimal", "low", "medium", "high", "xhigh", "max"];
/// Responses API `reasoning.summary` values.
pub(crate) const REASONING_SUMMARIES: &[&str] = &["auto", "concise", "detailed"];
/// Responses API web-search `search_context_size` values.
pub(crate) const WEB_SEARCH_CONTEXT_SIZES: &[&str] = &["low", "medium", "high"];

/// Reject a configuration value that is not one of the documented choices.
/// Comparison is exact (no case folding or trimming): the provider APIs are
/// exact, and a typo should fail at construction, not one request later.
pub(crate) fn validate_config_choice(field: &str, value: &str, allowed: &[&str]) -> LuaResult<()> {
    if allowed.contains(&value) {
        return Ok(());
    }
    Err(mlua::Error::external(IronCrewError::Validation(format!(
        "{field} must be one of: {} (got '{value}')",
        allowed.join(", ")
    ))))
}

#[cfg(test)]
mod tests {
    use super::validate_config_choice;

    #[test]
    fn config_choices_reject_unknown_values_and_name_the_allowed_set() {
        const EFFORTS: &[&str] = &["low", "medium", "high"];
        assert!(validate_config_choice("reasoning_effort", "low", EFFORTS).is_ok());
        let error = validate_config_choice("reasoning_effort", "lwo", EFFORTS)
            .unwrap_err()
            .to_string();
        assert!(error.contains("reasoning_effort"), "{error}");
        assert!(error.contains("'lwo'"), "{error}");
        assert!(error.contains("low, medium, high"), "{error}");
        // Case and whitespace are not normalized: the API is exact, so are we.
        assert!(validate_config_choice("reasoning_effort", "Low", EFFORTS).is_err());
        assert!(validate_config_choice("reasoning_effort", " low", EFFORTS).is_err());
    }
}
