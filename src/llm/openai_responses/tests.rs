use super::*;
use crate::engine::agent::ResponseFormat;

fn provider() -> OpenAiResponsesProvider {
    OpenAiResponsesProvider::new("k".into(), None, ResponsesConfig::default())
}

fn request(images: Option<Vec<ImageInput>>, format: Option<ResponseFormat>) -> ChatRequest {
    let mut user = ChatMessage::user("describe this");
    user.images = images;
    ChatRequest {
        messages: vec![user],
        model: "gpt-x".into(),
        temperature: None,
        max_tokens: None,
        response_format: format,
        prompt_cache_key: None,
        prompt_cache_retention: None,
        usage_tracker: None,
        reasoning_effort: None,
    }
}

#[test]
fn json_schema_response_format_is_sent_as_text_format() {
    let body = provider()
        .build_body(
            &request(
                None,
                Some(ResponseFormat::JsonSchema {
                    name: "verdict".into(),
                    schema: json!({"type": "object", "properties": {"ok": {"type": "boolean"}}}),
                }),
            ),
            None,
        )
        .unwrap();
    assert_eq!(body["text"]["format"]["type"], "json_schema");
    assert_eq!(body["text"]["format"]["name"], "verdict");
    assert_eq!(
        body["text"]["format"]["schema"]["properties"]["ok"]["type"],
        "boolean"
    );
    assert_eq!(body["text"]["format"]["strict"], true);
}

#[test]
fn json_object_response_format_is_mapped() {
    let body = provider()
        .build_body(&request(None, Some(ResponseFormat::JsonObject)), None)
        .unwrap();
    assert_eq!(body["text"]["format"]["type"], "json_object");
}

#[test]
fn absent_response_format_leaves_text_unset() {
    let body = provider().build_body(&request(None, None), None).unwrap();
    assert!(body.get("text").is_none());
}

#[test]
fn user_images_are_sent_as_input_image_parts() {
    let body = provider()
        .build_body(
            &request(
                Some(vec![ImageInput {
                    mime_type: "image/png".into(),
                    data: "QUJD".into(),
                }]),
                None,
            ),
            None,
        )
        .unwrap();
    let content = body["input"][0]["content"]
        .as_array()
        .expect("content parts");
    assert_eq!(content[0]["type"], "input_text");
    let image = content
        .iter()
        .find(|part| part["type"] == "input_image")
        .expect("image attachment must not be dropped");
    assert_eq!(image["image_url"], "data:image/png;base64,QUJD");
}

#[test]
fn text_only_messages_keep_a_single_text_part() {
    let body = provider().build_body(&request(None, None), None).unwrap();
    let content = body["input"][0]["content"]
        .as_array()
        .expect("content parts");
    assert_eq!(content.len(), 1);
    assert_eq!(content[0]["type"], "input_text");
}

#[test]
fn request_reasoning_effort_overrides_the_crew_level_config() {
    let crew_level = OpenAiResponsesProvider::new(
        "k".into(),
        None,
        ResponsesConfig {
            reasoning_effort: Some("low".into()),
            ..ResponsesConfig::default()
        },
    );
    let mut per_agent = request(None, None);
    per_agent.reasoning_effort = Some("high".into());
    let body = crew_level.build_body(&per_agent, None).unwrap();
    assert_eq!(body["reasoning"]["effort"], "high");

    // Without an agent value the crew-level config still applies.
    let body = crew_level.build_body(&request(None, None), None).unwrap();
    assert_eq!(body["reasoning"]["effort"], "low");
}

#[test]
fn luna_wire_defaults_to_low_with_function_and_server_tools() {
    let provider = OpenAiResponsesProvider::new(
        String::new(),
        None,
        ResponsesConfig {
            server_tools: vec![ServerTool::WebSearch { context_size: None }],
            ..Default::default()
        },
    );
    let mut request = request(None, None);
    request.model = "gpt-5.6-luna".into();
    request.max_tokens = Some(128);
    let tools = [ToolSchema {
        name: "lookup".into(),
        description: "lookup".into(),
        parameters: json!({}),
    }];
    let body = provider.build_body(&request, Some(&tools)).unwrap();
    assert_eq!(body["reasoning"]["effort"], "low");
    assert_eq!(body["max_output_tokens"], 128);
    assert_eq!(body["tools"].as_array().unwrap().len(), 2);
    for effort in ["low", "none", "minimal", "high"] {
        request.reasoning_effort = Some(effort.into());
        let body = provider.build_body(&request, Some(&tools));
        assert_eq!(
            provider.validate_request(&request, true).is_ok(),
            body.is_ok()
        );
        if let Ok(body) = body {
            assert_eq!(body["reasoning"]["effort"], effort);
        }
    }
}
