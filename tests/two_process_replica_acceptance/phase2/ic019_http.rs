use reqwest::header::{CACHE_CONTROL, CONTENT_TYPE, RETRY_AFTER};
use reqwest::{Response, StatusCode};

use super::*;

pub(super) async fn post_run(pair: &ProcessPair, base_url: &str, key: &str) -> Response {
    authenticated(pair.client.post(format!("{base_url}/flows/{FLOW}/run")))
        .header("Idempotency-Key", key)
        .json(&serde_json::json!({}))
        .send()
        .await
        .expect("send IC-019 run request")
}

pub(super) async fn accepted_run(response: Response) -> String {
    assert_eq!(response.status(), StatusCode::OK);
    assert!(response.headers().get("Idempotency-Replayed").is_none());
    let body: serde_json::Value = response.json().await.expect("parse IC-019 run acceptance");
    assert_eq!(body["status"], "started");
    assert_eq!(body["control_scope"], "process");
    body["run_id"]
        .as_str()
        .expect("accepted IC-019 run id")
        .to_owned()
}

pub(super) async fn start_conversation(pair: &ProcessPair, base_url: &str, id: &str) -> Response {
    authenticated(
        pair.client
            .post(format!("{base_url}/flows/{FLOW}/conversations/{id}/start")),
    )
    .json(&serde_json::json!({ "agent": "holder" }))
    .send()
    .await
    .expect("start IC-019 conversation")
}

pub(super) async fn assert_conversation_started(response: Response, id: &str) {
    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value = response
        .json()
        .await
        .expect("parse IC-019 conversation acceptance");
    assert_eq!(body["conversation_id"], id);
}

pub(super) async fn delete_conversation(pair: &ProcessPair, base_url: &str, id: &str) {
    let response = authenticated(
        pair.client
            .delete(format!("{base_url}/flows/{FLOW}/conversations/{id}")),
    )
    .send()
    .await
    .expect("delete IC-019 conversation");
    assert_eq!(response.status(), StatusCode::OK);
}

pub(super) async fn questions(pair: &ProcessPair, base_url: &str, run_id: &str) -> Response {
    authenticated(
        pair.client
            .get(format!("{base_url}/flows/{FLOW}/questions/{run_id}")),
    )
    .send()
    .await
    .expect("read IC-019 questions")
}

pub(super) async fn abort(pair: &ProcessPair, base_url: &str, run_id: &str) -> Response {
    authenticated(
        pair.client
            .post(format!("{base_url}/flows/{FLOW}/abort/{run_id}")),
    )
    .send()
    .await
    .expect("abort IC-019 run")
}

pub(super) async fn assert_abort_accepted(response: Response) {
    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value = response.json().await.expect("parse IC-019 abort response");
    assert!(
        matches!(
            body["status"].as_str(),
            Some("aborted" | "cancellation_requested")
        ),
        "unexpected IC-019 abort response: {body}"
    );
}

pub(super) async fn open_sse(pair: &ProcessPair, base_url: &str, run_id: &str) -> Response {
    let response = authenticated(
        pair.client
            .get(format!("{base_url}/flows/{FLOW}/events/{run_id}")),
    )
    .send()
    .await
    .expect("open IC-019 run SSE");
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        response.headers()[CONTENT_TYPE]
            .to_str()
            .expect("IC-019 SSE content type")
            .starts_with("text/event-stream")
    );
    assert_eq!(response.headers()[CACHE_CONTROL], "no-store, no-transform");
    response
}

pub(super) async fn assert_unavailable(response: Response, expected: &str) {
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body = response.text().await.expect("read IC-019 503 body");
    assert!(body.contains(expected), "unexpected 503 body: {body}");
}

pub(super) async fn assert_limited(response: Response, expected_error: &str) {
    let body = limited_body(response).await;
    assert_eq!(body["error"], expected_error);
}

pub(super) async fn assert_limited_contains(response: Response, expected: &str) {
    let body = limited_body(response).await;
    let error = body["error"].as_str().expect("IC-019 429 error text");
    assert!(error.contains(expected), "unexpected 429 error: {error}");
}

async fn limited_body(response: Response) -> serde_json::Value {
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(response.headers()[CACHE_CONTROL], "no-store");
    let retry_after = response.headers()[RETRY_AFTER]
        .to_str()
        .expect("IC-019 Retry-After text")
        .parse::<u64>()
        .expect("IC-019 Retry-After integer");
    assert!(retry_after >= 1, "Retry-After must be positive");
    response.json().await.expect("parse IC-019 429 body")
}

pub(super) async fn scrape(pair: &ProcessPair, base_url: &str) -> String {
    let response = authenticated(pair.client.get(format!("{base_url}/metrics")))
        .send()
        .await
        .expect("scrape IC-019 metrics");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[CACHE_CONTROL], "no-store");
    response.text().await.expect("read IC-019 metrics")
}

pub(super) fn sample(body: &str, series: &str) -> u64 {
    let prefix = format!("{series} ");
    let line = body
        .lines()
        .find(|line| line.starts_with(&prefix))
        .unwrap_or_else(|| panic!("missing metric series {series}"));
    line[prefix.len()..]
        .parse::<u64>()
        .unwrap_or_else(|error| panic!("invalid metric {series}: {error}"))
}

pub(super) fn assert_sample(body: &str, series: &str, expected: u64) {
    assert_eq!(sample(body, series), expected, "metric {series}");
}

pub(super) async fn wait_for_sample(
    pair: &ProcessPair,
    base_url: &str,
    series: &str,
    expected: u64,
) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        let body = scrape(pair, base_url).await;
        if sample(&body, series) == expected {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("metric {series} did not become {expected}");
}

pub(super) fn assert_metrics_hide_identities(
    body: &str,
    pair: &ProcessPair,
    forbidden_values: &[&str],
) {
    for forbidden in [
        API_TOKEN,
        "acceptance-client",
        pair.owner_a_id.as_str(),
        pair.database_url.as_str(),
        pair.prefix.as_str(),
        KEYRING_JSON,
        ACTIVE_KEY_ID,
        "AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE=",
    ]
    .into_iter()
    .chain(forbidden_values.iter().copied())
    {
        assert!(
            !body.contains(forbidden),
            "metrics exposed forbidden identity or key material"
        );
    }
}
