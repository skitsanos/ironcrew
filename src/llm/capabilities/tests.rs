use super::*;
use crate::engine::agent::Agent;

#[test]
fn snapshot_matching_uses_exact_provider_date_formats() {
    assert!(matches_model(
        "gpt-5.6-luna-2026-09-01",
        "gpt-5.6-luna",
        false
    ));
    assert!(!matches_model(
        "gpt-5.6-luna-+2026-9-1",
        "gpt-5.6-luna",
        false
    ));
    assert!(matches_model(
        "claude-opus-4-7-20260901",
        "claude-opus-4-7",
        true
    ));
    assert!(!matches_model(
        "claude-opus-4-7-20260230",
        "claude-opus-4-7",
        true
    ));
    assert!(!matches_model(
        "claude-opus-4-7-custom",
        "claude-opus-4-7",
        true
    ));
}

fn request(model: &str, effort: Option<&str>) -> ChatRequest {
    Agent {
        reasoning_effort: effort.map(str::to_owned),
        ..Default::default()
    }
    .chat_request(model.into(), vec![])
}

#[test]
fn luna_defaults_are_low_or_none_by_transport_and_tools() {
    for model in ["gpt-5.6-luna", "gpt-5.6-luna-2026-08-01"] {
        let request = request(model, None);
        for (transport, tools, expected, field) in [
            (
                Transport::ChatCompletions,
                false,
                "low",
                "max_completion_tokens",
            ),
            (
                Transport::ChatCompletions,
                true,
                "none",
                "max_completion_tokens",
            ),
            (Transport::Responses, false, "low", "max_output_tokens"),
            (Transport::Responses, true, "low", "max_output_tokens"),
        ] {
            let options = openai(
                transport,
                "https://api.openai.com/v1",
                &request,
                tools,
                None,
            )
            .unwrap();
            assert_eq!(options.effort, Some(expected));
            assert_eq!(options.token_field, field);
        }
    }
}

#[test]
fn family_matching_does_not_capture_custom_suffixes_or_invalid_dates() {
    for model in [
        "gpt-5.6-lunatic",
        "gpt-5.6-luna-custom",
        "gpt-5.6-luna-2026-02-30",
        "gpt-5.6-luna-2026-08-01-fine-tuned",
    ] {
        let request = request(model, Some("minimal"));
        let options = openai(
            Transport::ChatCompletions,
            "https://api.openai.com/v1",
            &request,
            true,
            None,
        )
        .unwrap();
        assert_eq!(options.effort, Some("minimal"));
        assert_eq!(options.token_field, "max_completion_tokens");
    }
}

#[test]
fn custom_endpoints_do_not_inherit_openai_semantics_from_model_names() {
    for url in [
        "https://api.openai.com.example/v1",
        "http://api.openai.com/v1",
        "https://api.openai.com:8443/v1",
        "https://proxy.example/v1",
        "https://api.openai.com/custom",
    ] {
        let request = request("gpt-5.6-luna", None);
        let options = openai(Transport::ChatCompletions, url, &request, true, None).unwrap();
        assert!(options.effort.is_none(), "{url}");
        assert_eq!(options.token_field, "max_tokens");
    }
}

#[test]
fn explicit_efforts_are_validated_and_never_downgraded() {
    for effort in ["none", "low", "medium", "high", "xhigh", "max"] {
        let request = request("gpt-5.6-luna", Some(effort));
        assert_eq!(
            openai(
                Transport::Responses,
                "https://api.openai.com",
                &request,
                true,
                Some("minimal")
            )
            .unwrap()
            .effort,
            Some(effort)
        );
        assert_eq!(
            openai(
                Transport::ChatCompletions,
                "https://api.openai.com/v1",
                &request,
                true,
                None
            )
            .is_ok(),
            effort == "none"
        );
    }
    for model in ["gpt-5.6-luna", "gpt-5.6-terra", "gpt-5.6-sol", "gpt-5.6"] {
        let request = request(model, None);
        assert!(
            openai(
                Transport::Responses,
                "https://api.openai.com",
                &request,
                false,
                Some("minimal")
            )
            .is_err()
        );
    }
}

#[test]
fn anthropic_manual_thinking_has_explicit_bounds_and_temperature_policy() {
    let mut request = request("claude-sonnet-4-6", None);
    assert!(anthropic("https://api.anthropic.com", &request, Some(1024), false).is_ok());
    for budget in [0, 1023, 1_000_001, u32::MAX] {
        assert!(anthropic("https://api.anthropic.com", &request, Some(budget), false).is_err());
    }
    request.max_tokens = Some(1024);
    assert!(anthropic("https://api.anthropic.com", &request, Some(1024), false).is_err());
    request.max_tokens = Some(1025);
    request.temperature = Some(0.7);
    assert!(anthropic("https://api.anthropic.com", &request, Some(1024), false).is_err());
    request.temperature = Some(1.0);
    assert!(anthropic("https://api.anthropic.com", &request, Some(1024), false).is_ok());
    request.model = "claude-opus-4-7".into();
    assert!(anthropic("https://api.anthropic.com", &request, Some(1024), false).is_err());
}
