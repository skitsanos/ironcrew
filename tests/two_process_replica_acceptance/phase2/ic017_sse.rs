use std::time::Duration;

use reqwest::{Response, StatusCode, header::HeaderValue};

use super::ic017_http::{assert_error_cache_policy, assert_no_forbidden, event_url};
use super::*;

const SSE_READ_ATTEMPTS: usize = 5;
const JOURNAL_UNAVAILABLE_BODY: &str = r#"{"error":"Run-event replay is temporarily unavailable"}"#;

pub(super) async fn get_sse_when_available(
    pair: &ProcessPair,
    run_id: &str,
    cursor: Option<HeaderValue>,
    context: &str,
) -> Response {
    for attempt in 1..=SSE_READ_ATTEMPTS {
        let mut request = authenticated(pair.client.get(event_url(pair, run_id)));
        if let Some(cursor) = cursor.as_ref() {
            request = request.header("Last-Event-ID", cursor.clone());
        }
        let response = request
            .send()
            .await
            .unwrap_or_else(|error| panic!("read IC-017 {context} through replica B: {error}"));
        if response.status() != StatusCode::SERVICE_UNAVAILABLE {
            return response;
        }

        assert_error_cache_policy(&response);
        let body = response
            .text()
            .await
            .unwrap_or_else(|error| panic!("read IC-017 {context} 503 body: {error}"));
        assert_no_forbidden(context, &body);
        if is_journal_temporarily_unavailable(StatusCode::SERVICE_UNAVAILABLE, &body)
            && attempt < SSE_READ_ATTEMPTS
        {
            tokio::time::sleep(Duration::from_millis(100 * attempt as u64)).await;
            continue;
        }
        panic!("IC-017 {context} attempt {attempt} returned 503: {body}");
    }
    unreachable!("bounded IC-017 SSE read loop must return or fail")
}

fn is_journal_temporarily_unavailable(status: StatusCode, body: &str) -> bool {
    status == StatusCode::SERVICE_UNAVAILABLE
        && serde_json::from_str::<serde_json::Value>(body).ok()
            == serde_json::from_str::<serde_json::Value>(JOURNAL_UNAVAILABLE_BODY).ok()
}

#[test]
fn successful_sse_retry_is_bounded_to_exact_journal_unavailability() {
    assert!(is_journal_temporarily_unavailable(
        StatusCode::SERVICE_UNAVAILABLE,
        JOURNAL_UNAVAILABLE_BODY,
    ));
    assert!(!is_journal_temporarily_unavailable(
        StatusCode::OK,
        JOURNAL_UNAVAILABLE_BODY,
    ));
    assert!(!is_journal_temporarily_unavailable(
        StatusCode::SERVICE_UNAVAILABLE,
        r#"{"error":"another failure"}"#,
    ));
}
