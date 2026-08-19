use super::*;

#[test]
fn parse_captures_thinking_blocks_verbatim() {
    let resp = json!({
        "content": [
            {"type": "thinking", "thinking": "let me think", "signature": "sig-abc"},
            {"type": "text", "text": "hello"},
            {"type": "tool_use", "id": "tu_1", "name": "search", "input": {"q": "x"}}
        ],
        "usage": {"input_tokens": 5, "output_tokens": 3}
    });
    let parsed = parse_anthropic_response(&resp, None).unwrap();
    let raw = parsed.raw_blocks.expect("thinking block captured");
    assert_eq!(raw.len(), 1);
    assert_eq!(raw[0]["type"], "thinking");
    // Signature preserved verbatim — the API rejects modified blocks.
    assert_eq!(raw[0]["signature"], "sig-abc");
    assert_eq!(raw[0]["thinking"], "let me think");
    assert_eq!(parsed.tool_calls.len(), 1);
    assert_eq!(parsed.reasoning.as_deref(), Some("let me think"));
}

#[test]
fn parse_without_thinking_has_no_raw_blocks() {
    let resp = json!({
        "content": [{"type": "text", "text": "hi"}],
        "usage": {"input_tokens": 1, "output_tokens": 1}
    });
    let parsed = parse_anthropic_response(&resp, None).unwrap();
    assert!(parsed.raw_blocks.is_none());
}

#[test]
fn build_body_replays_thinking_before_tool_use() {
    let provider = AnthropicProvider::new(
        "k".into(),
        None,
        AnthropicConfig {
            thinking_budget: Some(2048),
            server_tools: vec![],
        },
    );
    let thinking = json!({"type": "thinking", "thinking": "reasoning", "signature": "sig-1"});
    let assistant = ChatMessage::assistant_with_blocks(
        None,
        Some(vec![ToolCallRequest {
            id: "tu_1".into(),
            call_type: "function".into(),
            function: ToolCallFunction {
                name: "search".into(),
                arguments: "{\"q\":\"x\"}".into(),
            },
        }]),
        Some(vec![thinking]),
    );
    let req = ChatRequest {
        messages: vec![
            ChatMessage::user("hi"),
            assistant,
            ChatMessage::tool("tu_1", "result"),
        ],
        model: "claude-x".into(),
        temperature: None,
        max_tokens: None,
        response_format: None,
        prompt_cache_key: None,
        prompt_cache_retention: None,
    };
    let body = provider.build_body(&req, None);
    let messages = body["messages"].as_array().unwrap();
    let asst = messages
        .iter()
        .find(|m| m["role"] == "assistant")
        .expect("assistant message present");
    let blocks = asst["content"].as_array().unwrap();
    // Thinking block must come first, with its signature intact, before tool_use.
    assert_eq!(blocks[0]["type"], "thinking");
    assert_eq!(blocks[0]["signature"], "sig-1");
    let think_pos = blocks.iter().position(|b| b["type"] == "thinking").unwrap();
    let tool_pos = blocks.iter().position(|b| b["type"] == "tool_use").unwrap();
    assert!(
        think_pos < tool_pos,
        "thinking block must precede tool_use to satisfy Anthropic"
    );
}

fn schema_request(images: Option<Vec<ImageInput>>) -> ChatRequest {
    let mut user = ChatMessage::user("describe this");
    user.images = images;
    ChatRequest {
        messages: vec![user],
        model: "claude-x".into(),
        temperature: None,
        max_tokens: None,
        response_format: Some(ResponseFormat::JsonSchema {
            name: "verdict".into(),
            schema: json!({
                "type": "object",
                "properties": {"ok": {"type": "boolean"}},
                "required": ["ok"],
            }),
        }),
        prompt_cache_key: None,
        prompt_cache_retention: None,
    }
}

#[test]
fn json_schema_response_format_forces_a_schema_tool() {
    let provider = AnthropicProvider::new("k".into(), None, AnthropicConfig::default());
    let body = provider.build_body(&schema_request(None), None);

    let tools = body["tools"]
        .as_array()
        .expect("a schema tool must be defined for structured output");
    let schema_tool = tools
        .iter()
        .find(|tool| tool["name"] == "verdict")
        .expect("structured-output tool present");
    assert_eq!(
        schema_tool["input_schema"]["properties"]["ok"]["type"],
        "boolean"
    );
    assert_eq!(body["tool_choice"]["type"], "tool");
    assert_eq!(body["tool_choice"]["name"], "verdict");
}

#[test]
fn forced_schema_tool_call_is_returned_as_content_not_a_tool_call() {
    let resp = json!({
        "content": [
            {"type": "tool_use", "id": "tu_1", "name": "verdict", "input": {"ok": true}}
        ],
        "usage": {"input_tokens": 1, "output_tokens": 1}
    });
    let parsed = parse_anthropic_response(&resp, Some("verdict")).unwrap();
    assert!(
        parsed.tool_calls.is_empty(),
        "the structured-output tool must not surface as a tool call"
    );
    let content = parsed.content.expect("schema output becomes content");
    let value: serde_json::Value = serde_json::from_str(&content).expect("valid JSON");
    assert_eq!(value["ok"], true);
}

#[test]
fn real_tool_calls_still_surface_alongside_structured_output() {
    let resp = json!({
        "content": [
            {"type": "tool_use", "id": "tu_1", "name": "search", "input": {"q": "x"}},
            {"type": "tool_use", "id": "tu_2", "name": "verdict", "input": {"ok": false}}
        ],
        "usage": {"input_tokens": 1, "output_tokens": 1}
    });
    let parsed = parse_anthropic_response(&resp, Some("verdict")).unwrap();
    assert_eq!(parsed.tool_calls.len(), 1);
    assert_eq!(parsed.tool_calls[0].function.name, "search");
    assert!(parsed.content.unwrap().contains("false"));
}
