use std::collections::BTreeSet;

use reqwest::header::{CACHE_CONTROL, CONTENT_TYPE, HeaderValue};

use super::ic017_support::{JournalSnapshot, SseFrame};
use super::*;

const CURSOR_READ_ATTEMPTS: usize = 5;
const JOURNAL_UNAVAILABLE_BODY: &str = r#"{"error":"Run-event replay is temporarily unavailable"}"#;

pub(super) fn event_url(pair: &ProcessPair, run_id: &str) -> String {
    format!("{}/flows/{FLOW}/events/{run_id}", pair.survivor_b.base_url)
}

pub(super) fn assert_no_forbidden(label: &str, value: &str) {
    for (name, forbidden) in [
        ("first prompt", FIRST_PROMPT),
        ("second prompt", SECOND_PROMPT),
        ("first answer", FIRST_ANSWER),
        ("second answer", SECOND_ANSWER),
        ("first choice", "approve"),
        ("second choice", "reject"),
        ("third choice", "finish"),
        ("fourth choice", "hold"),
        ("API token", API_TOKEN),
        ("idempotency key", "ic017-process-cursor-key-0001"),
        ("keyring", KEYRING_JSON),
        (
            "key material",
            "AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE=",
        ),
    ] {
        assert!(
            !value.contains(forbidden),
            "{label} exposed forbidden {name}"
        );
    }
}

pub(super) fn assert_error_cache_policy(response: &Response) {
    assert_eq!(
        response
            .headers()
            .get(CACHE_CONTROL)
            .and_then(|value| value.to_str().ok()),
        Some("no-store")
    );
}

pub(super) fn assert_sse_cache_policy(response: &Response) {
    assert_eq!(
        response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok()),
        Some("text/event-stream")
    );
    assert_eq!(
        response
            .headers()
            .get(CACHE_CONTROL)
            .and_then(|value| value.to_str().ok()),
        Some("no-store, no-transform")
    );
    assert_eq!(
        response
            .headers()
            .get("x-accel-buffering")
            .and_then(|value| value.to_str().ok()),
        Some("no")
    );
}

pub(super) async fn assert_initial_http_edges(pair: &ProcessPair, run_id: &str) {
    let unauthorized = pair
        .client
        .get(event_url(pair, run_id))
        .header("Last-Event-ID", "malformed")
        .send()
        .await
        .expect("read unauthenticated IC-017 response");
    assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);
    assert_error_cache_policy(&unauthorized);
    let unauthorized = unauthorized
        .text()
        .await
        .expect("read unauthenticated IC-017 body");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&unauthorized).unwrap(),
        serde_json::json!({"error": "Missing Authorization header"})
    );
    assert_no_forbidden("authentication error", &unauthorized);

    for (cursor, message) in [
        (
            "malformed".to_string(),
            "run-event cursor must use '<run_id>:<sequence>'",
        ),
        (
            format!("{run_id}:0"),
            "run-event cursor sequence must be greater than zero",
        ),
        (
            format!("{run_id}:01"),
            "run-event cursor sequence must not contain leading zeroes",
        ),
    ] {
        assert_cursor_error(
            pair,
            run_id,
            HeaderValue::from_str(&cursor).expect("ASCII cursor"),
            StatusCode::BAD_REQUEST,
            "invalid_cursor",
            message,
        )
        .await;
    }
    assert_cursor_error(
        pair,
        run_id,
        HeaderValue::from_bytes(&[0xff]).expect("non-ASCII header value"),
        StatusCode::BAD_REQUEST,
        "invalid_cursor",
        "run-event cursor must contain valid ASCII",
    )
    .await;
    assert_cursor_error(
        pair,
        run_id,
        HeaderValue::from_static("different-run:1"),
        StatusCode::BAD_REQUEST,
        "cursor_cross_run",
        "run-event cursor belongs to a different run",
    )
    .await;
    assert_cursor_error(
        pair,
        run_id,
        HeaderValue::from_str(&format!("{run_id}:2")).unwrap(),
        StatusCode::CONFLICT,
        "cursor_ahead",
        "run-event cursor is ahead of the stream (latest sequence 1)",
    )
    .await;
}

pub(super) async fn assert_cursor_error(
    pair: &ProcessPair,
    run_id: &str,
    cursor: HeaderValue,
    status: StatusCode,
    code: &str,
    message: &str,
) -> String {
    for attempt in 1..=CURSOR_READ_ATTEMPTS {
        let response = authenticated(pair.client.get(event_url(pair, run_id)))
            .header("Last-Event-ID", cursor.clone())
            .send()
            .await
            .expect("read IC-017 cursor error through replica B");
        let response_status = response.status();
        assert_error_cache_policy(&response);
        let text = response.text().await.expect("read IC-017 cursor error");
        assert_no_forbidden("cursor error", &text);

        if should_retry_cursor_probe(status, response_status, &text, attempt) {
            tokio::time::sleep(Duration::from_millis(100 * attempt as u64)).await;
            continue;
        }

        assert_eq!(
            response_status, status,
            "IC-017 cursor probe attempt {attempt} returned {text}"
        );
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&text).expect("parse IC-017 cursor error"),
            serde_json::json!({"error": message, "code": code})
        );
        return text;
    }
    unreachable!("bounded IC-017 cursor probe loop must return or fail")
}

fn should_retry_cursor_probe(
    expected_status: StatusCode,
    response_status: StatusCode,
    body: &str,
    attempt: usize,
) -> bool {
    expected_status == StatusCode::CONFLICT
        && response_status == StatusCode::SERVICE_UNAVAILABLE
        && attempt < CURSOR_READ_ATTEMPTS
        && serde_json::from_str::<serde_json::Value>(body).ok()
            == serde_json::from_str::<serde_json::Value>(JOURNAL_UNAVAILABLE_BODY).ok()
}

#[test]
fn cursor_retry_is_bounded_to_exact_journal_unavailability() {
    assert!(should_retry_cursor_probe(
        StatusCode::CONFLICT,
        StatusCode::SERVICE_UNAVAILABLE,
        JOURNAL_UNAVAILABLE_BODY,
        1,
    ));
    assert!(!should_retry_cursor_probe(
        StatusCode::CONFLICT,
        StatusCode::SERVICE_UNAVAILABLE,
        JOURNAL_UNAVAILABLE_BODY,
        CURSOR_READ_ATTEMPTS,
    ));
    assert!(!should_retry_cursor_probe(
        StatusCode::CONFLICT,
        StatusCode::SERVICE_UNAVAILABLE,
        r#"{"error":"another failure"}"#,
        1,
    ));
    assert!(!should_retry_cursor_probe(
        StatusCode::CONFLICT,
        StatusCode::CONFLICT,
        JOURNAL_UNAVAILABLE_BODY,
        1,
    ));
    assert!(!should_retry_cursor_probe(
        StatusCode::BAD_REQUEST,
        StatusCode::SERVICE_UNAVAILABLE,
        JOURNAL_UNAVAILABLE_BODY,
        1,
    ));
}

pub(super) fn assert_exact_ids(frames: &[SseFrame], run_id: &str, sequences: &[u64]) {
    let expected: Vec<_> = sequences
        .iter()
        .map(|sequence| Some(format!("{run_id}:{sequence}")))
        .collect();
    assert_eq!(
        frames
            .iter()
            .map(|frame| frame.id.clone())
            .collect::<Vec<_>>(),
        expected
    );
    assert_eq!(
        frames
            .iter()
            .filter_map(|frame| frame.id.as_ref())
            .collect::<BTreeSet<_>>()
            .len(),
        frames.len(),
        "numbered SSE frames must not repeat"
    );
}

pub(super) fn assert_bounded_terminal(snapshot: &JournalSnapshot) {
    assert_eq!(snapshot.retained_events, 4);
    assert_eq!(snapshot.global_events, 4);
    assert_eq!(
        snapshot.eviction_reason.as_deref(),
        Some("writer_backpressure")
    );
    assert!(snapshot.journal_complete);
    assert_eq!(
        snapshot
            .rows
            .iter()
            .map(|row| (row.sequence, row.event_type.as_str()))
            .collect::<Vec<_>>(),
        vec![
            (4, "human_input_requested"),
            (5, "human_input_received"),
            (6, "log"),
            (7, "run_complete"),
        ]
    );
    assert_eq!(
        snapshot.retained_bytes,
        snapshot
            .rows
            .iter()
            .map(|row| row.accounted_bytes)
            .sum::<u64>()
    );
    assert_eq!(snapshot.retained_bytes, 4096);
    assert_eq!(snapshot.global_bytes, snapshot.retained_bytes);
    for row in &snapshot.rows {
        assert!(row.payload_bytes <= 1024);
        assert_eq!(row.accounted_bytes, 1024);
        assert_no_forbidden("durable journal row", &row.payload.to_string());
    }
}
