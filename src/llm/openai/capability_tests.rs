use super::*;
use crate::engine::agent::Agent;

fn tool() -> ToolSchema {
    ToolSchema {
        name: "lookup".into(),
        description: "lookup".into(),
        parameters: json!({}),
    }
}

#[test]
fn wire_defaults_and_validation_share_policy() {
    let provider = OpenAiProvider::new(String::new(), None);
    let mut request = Agent {
        max_tokens: Some(128),
        ..Default::default()
    }
    .chat_request("gpt-5.6-luna".into(), vec![]);
    let body = provider.build_body(&request, None).unwrap();
    assert_eq!(body["reasoning_effort"], "low");
    assert_eq!(body["max_completion_tokens"], 128);
    assert!(body.get("max_tokens").is_none());
    let body = provider.build_body(&request, Some(&[tool()])).unwrap();
    assert_eq!(body["reasoning_effort"], "none");
    for effort in ["none", "low", "minimal", "high"] {
        request.reasoning_effort = Some(effort.into());
        for has_tools in [false, true] {
            let tools = [tool()];
            let body = provider.build_body(&request, has_tools.then_some(&tools));
            assert_eq!(
                provider.validate_request(&request, has_tools).is_ok(),
                body.is_ok()
            );
            if let Ok(body) = body {
                assert_eq!(body["reasoning_effort"], effort);
            }
        }
    }
}

#[test]
fn custom_endpoint_keeps_explicit_options_and_legacy_token_field() {
    let provider = OpenAiProvider::new(String::new(), Some("https://custom.example/v1".into()));
    let request = Agent {
        max_tokens: Some(128),
        temperature: Some(0.7),
        reasoning_effort: Some("low".into()),
        ..Default::default()
    }
    .chat_request("gpt-5.6-luna".into(), vec![]);
    let body = provider.build_body(&request, Some(&[tool()])).unwrap();
    assert_eq!(body["reasoning_effort"], "low");
    assert_eq!(body["temperature"], 0.7_f32);
    assert_eq!(body["max_tokens"], 128);
    assert!(body.get("max_completion_tokens").is_none());
}
