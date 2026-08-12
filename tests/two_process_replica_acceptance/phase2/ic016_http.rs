use reqwest::StatusCode;
use reqwest::header::{CACHE_CONTROL, CONTENT_TYPE};

use super::*;

pub(super) async fn list_once(pair: &ProcessPair, run_id: &str, prompt: &str) -> String {
    let response = authenticated(pair.client.get(format!(
        "{}/flows/{FLOW}/questions/{run_id}",
        pair.survivor_b.base_url
    )))
    .send()
    .await
    .expect("list IC-016 question through peer revision");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[CACHE_CONTROL], "no-store");
    let body: serde_json::Value = response.json().await.expect("parse IC-016 question list");
    assert_eq!(body["status"], "waiting_for_input");
    assert_eq!(body["owner_instance_id"], pair.owner_a_id);
    assert_eq!(body["control_scope"], "shared_store");
    let questions = body["questions"]
        .as_array()
        .expect("IC-016 questions array");
    assert_eq!(questions.len(), 1);
    assert_eq!(questions[0]["prompt"], prompt);
    questions[0]["question_id"]
        .as_str()
        .expect("IC-016 question id")
        .to_owned()
}

pub(super) async fn assert_unsafe_routes_fail(
    pair: &ProcessPair,
    run_id: &str,
    question_id: &str,
    listener_live: bool,
    forbidden: &[&str],
) {
    let list = authenticated(pair.client.get(format!(
        "{}/flows/{FLOW}/questions/{run_id}",
        pair.survivor_b.base_url
    )))
    .send()
    .await;
    let answer = authenticated(pair.client.post(format!(
        "{}/flows/{FLOW}/answer/{run_id}",
        pair.survivor_b.base_url
    )))
    .json(&serde_json::json!({
        "question_id": question_id,
        "answer": "rotation-approved",
    }))
    .send()
    .await;

    if listener_live {
        let list = list.expect("IC-016 rejected replica list response");
        assert_eq!(list.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(list.headers()[CACHE_CONTROL], "no-store");
        let list_body = list.text().await.expect("read IC-016 rejected list");
        assert!(list_body.contains("temporarily unavailable"));
        assert_hidden(&list_body, forbidden);

        let answer = answer.expect("IC-016 rejected replica answer response");
        assert_eq!(answer.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(answer.headers()[CACHE_CONTROL], "no-store");
        let answer_body = answer.text().await.expect("read IC-016 rejected answer");
        assert!(answer_body.contains("temporarily unavailable"));
        assert_hidden(&answer_body, forbidden);
    } else {
        assert!(
            list.is_err(),
            "stopped IC-016 replica served a question list"
        );
        assert!(answer.is_err(), "stopped IC-016 replica accepted an answer");
    }
}

pub(super) async fn answer_once(pair: &ProcessPair, run_id: &str, question_id: &str, answer: &str) {
    let (status, body) = answer_question(
        &pair.client,
        &pair.survivor_b.base_url,
        run_id,
        question_id,
        answer,
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let body: serde_json::Value = serde_json::from_str(&body).expect("parse IC-016 answer ack");
    assert_eq!(body["run_id"], run_id);
    assert_eq!(body["question_id"], question_id);
    assert_eq!(body["status"], "queued");
    assert_eq!(body["owner_instance_id"], pair.owner_a_id);
    assert_eq!(body["control_scope"], "shared_store");

    let (repeat_status, repeat_body) = answer_question(
        &pair.client,
        &pair.survivor_b.base_url,
        run_id,
        question_id,
        answer,
    )
    .await;
    assert_eq!(repeat_status, StatusCode::NOT_FOUND);
    assert!(!repeat_body.contains(answer));
}

pub(super) async fn assert_durable_sse_hides(pair: &ProcessPair, run_id: &str, forbidden: &[&str]) {
    let response = authenticated(pair.client.get(format!(
        "{}/flows/{FLOW}/events/{run_id}",
        pair.survivor_b.base_url
    )))
    .send()
    .await
    .expect("read IC-016 durable SSE through peer");
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        response.headers()[CONTENT_TYPE]
            .to_str()
            .expect("IC-016 SSE content type")
            .starts_with("text/event-stream")
    );
    assert_eq!(response.headers()[CACHE_CONTROL], "no-store, no-transform");
    let body = response.text().await.expect("read IC-016 durable SSE body");
    assert!(body.contains("event: human_input_requested"));
    assert!(body.contains("event: run_complete"));
    assert_hidden(&body, forbidden);
}

pub(super) fn assert_raw_logs_hide(pair: &ProcessPair, forbidden: &[&str]) {
    let entries = std::fs::read_dir(pair._workspace.path()).expect("read IC-016 process logs");
    for entry in entries {
        let path = entry.expect("read IC-016 log entry").path();
        if path.extension().and_then(|value| value.to_str()) != Some("log") {
            continue;
        }
        let body = std::fs::read_to_string(path).expect("read IC-016 raw process log");
        assert_hidden(&body, forbidden);
    }
}

fn assert_hidden(surface: &str, forbidden: &[&str]) {
    for value in forbidden {
        assert!(
            !surface.contains(value),
            "IC-016 response, event, or log exposed a forbidden value"
        );
    }
}
