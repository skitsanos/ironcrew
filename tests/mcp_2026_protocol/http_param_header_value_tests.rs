use ironcrew::mcp::client::McpClient;
use serde_json::{Value, json};

use super::{
    boundary_test_support::isolate_environment, header_http_fixture::HeaderHttpFixture,
    http_param_header_tests::config, param_header_schemas::integer_tool,
};

#[tokio::test]
async fn integer_values_normalize_and_invalid_values_fail_locally_without_poisoning() {
    isolate_environment();
    let fixture = HeaderHttpFixture::spawn(json!([integer_tool()]), false).await;
    let client = McpClient::connect(&config(&fixture.url)).await.unwrap();
    client.list_all_tools().await.unwrap();

    let decimal = json!({"minimum": 42.0, "maximum": 42.0});
    let exponent: Value = serde_json::from_str(r#"{"minimum":4.2e1,"maximum":-0.0}"#).unwrap();
    assert!(decimal["minimum"].as_i64().is_none());
    assert!(exponent["minimum"].as_i64().is_none());
    for arguments in [
        decimal,
        exponent,
        json!({
            "minimum": -9_007_199_254_740_991_i64,
            "maximum": 9_007_199_254_740_991_i64
        }),
    ] {
        client.call_tool("integer", arguments).await.unwrap();
    }

    for (arguments, expected) in [
        (
            json!({"minimum": 42.5, "maximum": 0}),
            "does not match its annotated primitive type",
        ),
        (
            json!({"minimum": -9_007_199_254_740_992_i64, "maximum": 0}),
            "outside the JavaScript safe integer range",
        ),
        (
            json!({"minimum": 0, "maximum": 9_007_199_254_740_992_i64}),
            "outside the JavaScript safe integer range",
        ),
    ] {
        let error = client
            .call_tool("integer", arguments)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains(expected), "{error}");
    }

    client
        .call_tool("integer", json!({"minimum": 0, "maximum": 0}))
        .await
        .expect("local value rejection must not poison the connection");
    client.shutdown().await;

    let requests = fixture.requests.lock().unwrap().clone();
    let calls: Vec<_> = requests
        .iter()
        .filter(|request| request.body["method"] == "tools/call")
        .collect();
    assert_eq!(calls.len(), 4, "three invalid values must stay off wire");
    assert_headers(calls[0], "42", "42");
    assert_headers(calls[1], "42", "0");
    assert_headers(calls[2], "-9007199254740991", "9007199254740991");
    assert_headers(calls[3], "0", "0");
    fixture.shutdown().await;
}

fn assert_headers(
    request: &super::header_http_fixture::HeaderRequest,
    minimum: &str,
    maximum: &str,
) {
    assert_eq!(request.header("mcp-param-minimum"), minimum);
    assert_eq!(request.header("mcp-param-maximum"), maximum);
}
