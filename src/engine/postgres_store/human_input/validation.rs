use crate::utils::error::{IronCrewError, Result};

pub(in crate::engine::postgres_store) fn validate_human_input_route(
    label: &str,
    value: &str,
    max_bytes: usize,
) -> Result<()> {
    if value.is_empty() || value.len() > max_bytes || value.chars().any(char::is_control) {
        return Err(IronCrewError::Validation(format!(
            "Human-input {label} must be 1..={max_bytes} printable bytes"
        )));
    }
    Ok(())
}
