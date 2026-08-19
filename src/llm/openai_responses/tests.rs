use super::*;

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
    }
}

#[test]
fn json_schema_response_format_is_sent_as_text_format() {
    let body = provider().build_body(
        &request(
            None,
            Some(ResponseFormat::JsonSchema {
                name: "verdict".into(),
                schema: json!({"type": "object", "properties": {"ok": {"type": "boolean"}}}),
            }),
        ),
        None,
    );
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
    let body = provider().build_body(&request(None, Some(ResponseFormat::JsonObject)), None);
    assert_eq!(body["text"]["format"]["type"], "json_object");
}

#[test]
fn absent_response_format_leaves_text_unset() {
    let body = provider().build_body(&request(None, None), None);
    assert!(body.get("text").is_none());
}

#[test]
fn user_images_are_sent_as_input_image_parts() {
    let body = provider().build_body(
        &request(
            Some(vec![ImageInput {
                mime_type: "image/png".into(),
                data: "QUJD".into(),
            }]),
            None,
        ),
        None,
    );
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
    let body = provider().build_body(&request(None, None), None);
    let content = body["input"][0]["content"]
        .as_array()
        .expect("content parts");
    assert_eq!(content.len(), 1);
    assert_eq!(content[0]["type"], "input_text");
}
