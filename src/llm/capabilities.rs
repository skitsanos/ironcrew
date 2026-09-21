//! One offline policy for construction checks and wire serialization.
//! See docs/model-capabilities.md for sources, scope, and verification date.

use super::provider::ChatRequest;
use crate::utils::error::{IronCrewError, Result};

mod catalog;
#[cfg(test)]
mod tests;

pub(crate) const REVISION: &str = "2026-09-20.1";
pub(crate) const EFFORT_NAMES: &[&str] =
    &["none", "minimal", "low", "medium", "high", "xhigh", "max"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Transport {
    ChatCompletions,
    Responses,
}

#[derive(Clone, Copy)]
enum Temperature {
    ProviderChecked,
    DefaultOnly,
}

#[derive(Clone, Copy)]
struct ModelPolicy {
    aliases: &'static [&'static str],
    efforts: Option<&'static [&'static str]>,
    default_effort: Option<&'static str>,
    temperature: Temperature,
    chat_tools_require_none: bool,
}

pub(crate) struct Resolved<'a> {
    pub effort: Option<&'a str>,
    pub token_field: &'static str,
}

fn canonical_host(base_url: &str, host: &str) -> bool {
    reqwest::Url::parse(base_url).is_ok_and(|url| {
        url.scheme() == "https"
            && url.host_str() == Some(host)
            && url.port_or_known_default() == Some(443)
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none()
            && matches!(url.path().trim_end_matches('/'), "" | "/v1")
    })
}

/// Match only exact aliases or a real date in the provider's snapshot format.
fn matches_model(model: &str, alias: &str, compact_date: bool) -> bool {
    model == alias
        || model
            .strip_prefix(alias)
            .and_then(|suffix| suffix.strip_prefix('-'))
            .is_some_and(|date| {
                let (length, format) = if compact_date {
                    (8, "%Y%m%d")
                } else {
                    (10, "%Y-%m-%d")
                };
                date.len() == length
                    && date.bytes().enumerate().all(|(index, byte)| {
                        if !compact_date && matches!(index, 4 | 7) {
                            byte == b'-'
                        } else {
                            byte.is_ascii_digit()
                        }
                    })
                    && chrono::NaiveDate::parse_from_str(date, format).is_ok()
            })
}

fn invalid(model: &str, message: &str) -> IronCrewError {
    IronCrewError::Validation(format!("Model '{model}': {message}"))
}

fn validate_temperature(request: &ChatRequest, policy: Temperature) -> Result<()> {
    if let Some(value) = request.temperature {
        if !value.is_finite() || !(0.0..=2.0).contains(&value) {
            return Err(invalid(
                &request.model,
                "temperature must be finite and between 0 and 2",
            ));
        }
        if matches!(policy, Temperature::DefaultOnly) && value != 1.0 {
            return Err(invalid(
                &request.model,
                "temperature supports only the provider default (1); omit temperature",
            ));
        }
    }
    Ok(())
}

pub(crate) fn openai<'a>(
    transport: Transport,
    base_url: &str,
    request: &'a ChatRequest,
    has_tools: bool,
    configured_effort: Option<&'a str>,
) -> Result<Resolved<'a>> {
    let official = canonical_host(base_url, "api.openai.com");
    let policy = official
        .then(|| {
            catalog::OPENAI.iter().find(|policy| {
                policy
                    .aliases
                    .iter()
                    .any(|alias| matches_model(&request.model, alias, false))
            })
        })
        .flatten();
    let explicit = request.reasoning_effort.as_deref().or(configured_effort);
    if let Some(value) = explicit {
        let allowed = policy.and_then(|p| p.efforts).unwrap_or(EFFORT_NAMES);
        if !allowed.contains(&value) {
            return Err(invalid(
                &request.model,
                &format!(
                    "reasoning_effort '{value}' is unsupported; expected {}",
                    allowed.join(", ")
                ),
            ));
        }
    }
    let tools_require_none = transport == Transport::ChatCompletions
        && has_tools
        && policy.is_some_and(|p| p.chat_tools_require_none);
    if tools_require_none && explicit.is_some_and(|effort| effort != "none") {
        return Err(invalid(
            &request.model,
            "reasoning_effort with tools must be 'none' on Chat Completions; use provider = \"openai-responses\" for reasoning with tools",
        ));
    }
    validate_temperature(
        request,
        policy.map_or(Temperature::ProviderChecked, |p| p.temperature),
    )?;
    Ok(Resolved {
        effort: explicit.or_else(|| {
            if tools_require_none {
                Some("none")
            } else {
                policy.and_then(|p| p.default_effort)
            }
        }),
        token_field: match transport {
            Transport::Responses => "max_output_tokens",
            Transport::ChatCompletions if official => "max_completion_tokens",
            Transport::ChatCompletions => "max_tokens",
        },
    })
}

/// The Messages adapter implements manual thinking, not output_config.effort.
pub(crate) fn anthropic(
    base_url: &str,
    request: &ChatRequest,
    budget: Option<u32>,
    has_tools: bool,
) -> Result<()> {
    if request.reasoning_effort.is_some() {
        return Err(invalid(
            &request.model,
            "reasoning_effort is not supported by the anthropic adapter; use thinking_budget on a model supporting manual thinking",
        ));
    }
    let adaptive_only = canonical_host(base_url, "api.anthropic.com")
        && catalog::ANTHROPIC_ADAPTIVE_ONLY
            .iter()
            .any(|alias| matches_model(&request.model, alias, true));
    if budget.is_some() && adaptive_only {
        return Err(invalid(
            &request.model,
            "thinking_budget is unsupported: this model requires adaptive thinking, which this adapter does not implement",
        ));
    }
    if (budget.is_some() || adaptive_only)
        && !has_tools
        && matches!(
            request.response_format,
            Some(crate::engine::agent::ResponseFormat::JsonSchema { .. })
        )
    {
        return Err(invalid(
            &request.model,
            "manual or adaptive thinking cannot force the JSON-schema output tool; use real tools with automatic tool choice, or disable thinking on a supported model",
        ));
    }
    let temperature = if budget.is_some() || adaptive_only {
        Temperature::DefaultOnly
    } else {
        Temperature::ProviderChecked
    };
    validate_temperature(request, temperature)?;
    if request.temperature.is_some_and(|value| value > 1.0) {
        return Err(invalid(
            &request.model,
            "anthropic temperature must be between 0 and 1",
        ));
    }
    if let Some(budget) = budget {
        if !(1024..=1_000_000).contains(&budget) {
            return Err(invalid(
                &request.model,
                "thinking_budget must be between 1024 and 1000000",
            ));
        }
        if request.max_tokens.is_some_and(|max| max <= budget) {
            return Err(invalid(
                &request.model,
                "max_tokens must exceed thinking_budget",
            ));
        }
    }
    Ok(())
}
