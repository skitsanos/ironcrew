use reqwest::header::CACHE_CONTROL;

use super::*;

const INSTANCE_HEADER: &str = "x-ironcrew-instance-id";
const REPLAY_HEADER: &str = "idempotency-replayed";

fn assert_boundary(response: &Response, receiver: &str) {
    assert_eq!(
        response
            .headers()
            .get(INSTANCE_HEADER)
            .and_then(|value| value.to_str().ok()),
        Some(receiver),
        "IC-008 response receiver"
    );
    assert_eq!(
        response
            .headers()
            .get(CACHE_CONTROL)
            .and_then(|value| value.to_str().ok()),
        Some("no-store"),
        "IC-008 cache policy"
    );
}

pub(super) async fn instance_id(pair: &ProcessPair, base_url: &str) -> String {
    let response = authenticated(pair.client.get(format!("{base_url}/capabilities")))
        .send()
        .await
        .expect("read IC-008 capabilities");
    assert_eq!(response.status(), StatusCode::OK);
    let receiver = response.headers()[INSTANCE_HEADER]
        .to_str()
        .expect("IC-008 capability receiver")
        .to_owned();
    assert_boundary(&response, &receiver);
    let body: serde_json::Value = response.json().await.expect("parse IC-008 capabilities");
    assert_eq!(body["instance_id"], receiver);
    receiver
}

pub(super) async fn start(
    pair: &ProcessPair,
    base_url: &str,
    receiver: &str,
    id: &str,
) -> serde_json::Value {
    let response = start_response(pair, base_url, id).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_boundary(&response, receiver);
    let body: serde_json::Value = response.json().await.expect("parse IC-008 start");
    assert_eq!(body["conversation_id"], id);
    assert_eq!(body["flow"], FLOW);
    assert_eq!(body["agent"], "coordinator");
    body
}

pub(super) async fn start_response(pair: &ProcessPair, base_url: &str, id: &str) -> Response {
    authenticated(
        pair.client
            .post(format!("{base_url}/flows/{FLOW}/conversations/{id}/start")),
    )
    .json(&serde_json::json!({ "agent": "coordinator", "max_history": 20 }))
    .send()
    .await
    .expect("start IC-008 conversation")
}

pub(super) async fn message(
    pair: &ProcessPair,
    base_url: &str,
    id: &str,
    content: &str,
    key: Option<&str>,
) -> Response {
    message_with_client(pair.client.clone(), base_url, id, content, key).await
}

async fn message_with_client(
    client: Client,
    base_url: &str,
    id: &str,
    content: &str,
    key: Option<&str>,
) -> Response {
    let mut request = authenticated(client.post(format!(
        "{base_url}/flows/{FLOW}/conversations/{id}/messages"
    )))
    .json(&serde_json::json!({ "content": content }));
    if let Some(key) = key {
        request = request.header("Idempotency-Key", key);
    }
    request.send().await.expect("send IC-008 message")
}

pub(super) fn spawn_message(
    pair: &ProcessPair,
    base_url: String,
    id: &'static str,
    content: &'static str,
    key: &'static str,
) -> tokio::task::JoinHandle<Response> {
    let client = pair.client.clone();
    tokio::spawn(
        async move { message_with_client(client, &base_url, id, content, Some(key)).await },
    )
}

pub(super) async fn successful_message(
    response: Response,
    receiver: &str,
    replayed: bool,
    content: &str,
) -> serde_json::Value {
    assert_eq!(response.status(), StatusCode::OK);
    assert_boundary(&response, receiver);
    let replay = response
        .headers()
        .get(REPLAY_HEADER)
        .and_then(|value| value.to_str().ok());
    assert_eq!(replay, replayed.then_some("true"));
    let body: serde_json::Value = response.json().await.expect("parse IC-008 message");
    assert_eq!(body["assistant"], format!("mock:{content}"));
    body
}

pub(super) async fn history(
    pair: &ProcessPair,
    base_url: &str,
    receiver: &str,
    id: &str,
) -> serde_json::Value {
    let response = authenticated(pair.client.get(format!(
        "{base_url}/flows/{FLOW}/conversations/{id}/history"
    )))
    .send()
    .await
    .expect("read IC-008 history");
    assert_eq!(response.status(), StatusCode::OK);
    assert_boundary(&response, receiver);
    response.json().await.expect("parse IC-008 history")
}

pub(super) async fn assert_error(
    response: Response,
    receiver: &str,
    status: StatusCode,
    expected: &str,
) -> serde_json::Value {
    assert_eq!(response.status(), status);
    assert_boundary(&response, receiver);
    let body: serde_json::Value = response.json().await.expect("parse IC-008 error");
    assert_eq!(body["error"], expected);
    body
}

pub(super) async fn assert_error_contains(
    response: Response,
    receiver: &str,
    status: StatusCode,
    expected: &str,
) {
    assert_eq!(response.status(), status);
    assert_boundary(&response, receiver);
    let body: serde_json::Value = response.json().await.expect("parse IC-008 error");
    let error = body["error"].as_str().expect("IC-008 error text");
    assert!(error.contains(expected), "unexpected IC-008 error: {error}");
}

pub(super) async fn sse_conflict(
    pair: &ProcessPair,
    base_url: &str,
    receiver: &str,
    id: &str,
) -> serde_json::Value {
    let response = authenticated(
        pair.client
            .get(format!("{base_url}/flows/{FLOW}/conversations/{id}/events")),
    )
    .header("Last-Event-ID", "unsupported-shared-cursor")
    .send()
    .await
    .expect("read IC-008 conversation SSE boundary");
    assert_error(
        response,
        receiver,
        StatusCode::CONFLICT,
        "Conversation SSE replay is unavailable with shared-store coordination; use durable history for recovery",
    )
    .await
}

pub(super) async fn delete(
    pair: &ProcessPair,
    base_url: &str,
    receiver: &str,
    id: &str,
) -> Response {
    let response = authenticated(
        pair.client
            .delete(format!("{base_url}/flows/{FLOW}/conversations/{id}")),
    )
    .send()
    .await
    .expect("delete IC-008 conversation");
    assert_boundary(&response, receiver);
    response
}
