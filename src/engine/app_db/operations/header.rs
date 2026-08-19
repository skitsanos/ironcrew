//! Operation header parsing: names and parameters.

use crate::utils::error::{IronCrewError, Result};

use super::{MAX_OP_NAME_BYTES, MAX_PARAMS, ParamType};

pub(super) fn op_error(name: &str, message: impl std::fmt::Display) -> IronCrewError {
    IronCrewError::Validation(format!("postgres operation '{name}': {message}"))
}

pub(super) fn validate_op_name(name: &str) -> Result<()> {
    if name.is_empty() || name.len() > MAX_OP_NAME_BYTES {
        return Err(IronCrewError::Validation(format!(
            "postgres operation name must be 1..={MAX_OP_NAME_BYTES} bytes"
        )));
    }
    if !name
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return Err(IronCrewError::Validation(format!(
            "postgres operation name '{name}' must contain only ASCII letters, digits, '_' or '-'"
        )));
    }
    Ok(())
}

pub(super) fn parse_params(name: &str, line: &str) -> Result<Vec<(String, ParamType)>> {
    let mut params = Vec::new();
    let body = line.trim();
    if body.is_empty() {
        return Ok(params);
    }
    for entry in body.split(',') {
        let mut pieces = entry.split_whitespace();
        let (Some(param_name), Some(type_name), None) =
            (pieces.next(), pieces.next(), pieces.next())
        else {
            return Err(op_error(name, format!("malformed params entry '{entry}'")));
        };
        if !param_name
            .bytes()
            .enumerate()
            .all(|(i, b)| b == b'_' || b.is_ascii_alphanumeric() && (i > 0 || !b.is_ascii_digit()))
        {
            return Err(op_error(name, format!("invalid param name '{param_name}'")));
        }
        let Some(param_type) = ParamType::parse(type_name) else {
            return Err(op_error(
                name,
                format!(
                    "unknown param type '{type_name}'; supported: text, integer, double, boolean, json"
                ),
            ));
        };
        if params.iter().any(|(existing, _)| existing == param_name) {
            return Err(op_error(name, format!("duplicate param '{param_name}'")));
        }
        if params.len() >= MAX_PARAMS {
            return Err(op_error(name, format!("more than {MAX_PARAMS} params")));
        }
        params.push((param_name.to_string(), param_type));
    }
    Ok(params)
}
