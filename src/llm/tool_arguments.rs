use serde_json::Value;

pub(crate) const INVALID_TOOL_ARGUMENTS_RESULT: &str = "Tool error: invalid JSON arguments";

pub(crate) fn parse(arguments: &str) -> Result<Value, &'static str> {
    serde_json::from_str(arguments).map_err(|_| INVALID_TOOL_ARGUMENTS_RESULT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn malformed_arguments_return_the_bounded_provider_result() {
        assert_eq!(
            parse(r#"{"secret":"not-closed""#),
            Err(INVALID_TOOL_ARGUMENTS_RESULT)
        );
    }

    #[test]
    fn valid_arguments_are_preserved() {
        assert_eq!(parse(r#"{"value":42}"#).unwrap()["value"], 42);
    }
}
