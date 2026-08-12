use reqwest::header::CONTENT_TYPE;

use super::super::*;
use super::http;

pub(super) async fn assert_reads_observable(pair: &ProcessPair, run_id: &str, question_id: &str) {
    let owner = &pair.owner_a.base_url;
    let questions = authenticated(
        pair.client
            .get(format!("{owner}/flows/{FLOW}/questions/{run_id}")),
    )
    .send()
    .await
    .expect("read IC-020 draining questions");
    assert_eq!(questions.status(), StatusCode::OK);
    http::assert_receiver(&questions, &pair.owner_a_id);
    let body: serde_json::Value = questions.json().await.expect("parse IC-020 questions");
    assert!(body["questions"].as_array().is_some_and(|questions| {
        questions
            .iter()
            .any(|question| question["question_id"] == question_id)
    }));

    let run = authenticated(
        pair.client
            .get(format!("{owner}/flows/{FLOW}/runs/{run_id}")),
    )
    .send()
    .await
    .expect("read IC-020 draining run");
    assert_eq!(run.status(), StatusCode::OK);
    http::assert_receiver(&run, &pair.owner_a_id);
    assert_eq!(
        run.json::<serde_json::Value>().await.unwrap()["status"],
        "WaitingForInput"
    );

    let mut sse = authenticated(
        pair.client
            .get(format!("{owner}/flows/{FLOW}/events/{run_id}")),
    )
    .send()
    .await
    .expect("open IC-020 draining SSE");
    assert_eq!(sse.status(), StatusCode::OK);
    http::assert_receiver(&sse, &pair.owner_a_id);
    assert!(
        sse.headers()[CONTENT_TYPE]
            .to_str()
            .unwrap()
            .starts_with("text/event-stream")
    );
    let events = read_sse_until(&mut sse, "event: human_input_requested").await;
    assert!(events.contains("event: human_input_requested"));
    drop(sse);

    let metrics = authenticated(pair.client.get(format!("{owner}/metrics")))
        .send()
        .await
        .expect("scrape IC-020 draining metrics");
    assert_eq!(metrics.status(), StatusCode::OK);
    http::assert_receiver(&metrics, &pair.owner_a_id);
    let metrics = metrics.text().await.expect("read IC-020 metrics");
    for (state, value) in [
        ("accepting", 0),
        ("fencing", 0),
        ("draining", 1),
        ("stopping", 0),
    ] {
        assert!(
            metrics.lines().any(|line| {
                line == format!("ironcrew_process_lifecycle_state{{state=\"{state}\"}} {value}")
            }),
            "missing lifecycle metric {state}={value}"
        );
    }
    for (class, minimum) in [("work", 1_u64), ("control", 3)] {
        let prefix = format!("ironcrew_process_lifecycle_rejections_total{{class=\"{class}\"}} ");
        let value = metrics
            .lines()
            .find_map(|line| line.strip_prefix(&prefix))
            .expect("IC-020 lifecycle rejection counter")
            .parse::<u64>()
            .expect("IC-020 numeric lifecycle rejection counter");
        assert!(value >= minimum, "{class} lifecycle rejections: {value}");
    }
}

pub(super) async fn assert_replay(
    pair: &ProcessPair,
    peer_id: &str,
    key: &str,
    run_id: &str,
    owner_id: &str,
) {
    let response = authenticated(
        pair.client
            .post(format!("{}/flows/{FLOW}/run", pair.survivor_b.base_url)),
    )
    .header("Idempotency-Key", key)
    .json(&serde_json::json!({}))
    .send()
    .await
    .expect("replay IC-020 run through peer");
    assert_eq!(response.status(), StatusCode::OK);
    http::assert_receiver(&response, peer_id);
    assert_eq!(response.headers()["Idempotency-Replayed"], "true");
    let body: serde_json::Value = response.json().await.expect("parse IC-020 replay");
    assert_eq!(body["run_id"], run_id);
    assert_eq!(body["owner_instance_id"], owner_id);
}

pub(super) async fn assert_terminal_observable(pair: &ProcessPair, peer_id: &str, run_id: &str) {
    let peer = &pair.survivor_b.base_url;
    let questions = authenticated(
        pair.client
            .get(format!("{peer}/flows/{FLOW}/questions/{run_id}")),
    )
    .send()
    .await
    .expect("query IC-020 terminal mailbox");
    assert_eq!(questions.status(), StatusCode::NOT_FOUND);
    http::assert_receiver(&questions, peer_id);

    let events = authenticated(
        pair.client
            .get(format!("{peer}/flows/{FLOW}/events/{run_id}")),
    )
    .send()
    .await
    .expect("read IC-020 terminal SSE");
    assert_eq!(events.status(), StatusCode::OK);
    http::assert_receiver(&events, peer_id);
    let events = events.text().await.expect("read IC-020 terminal events");
    assert_eq!(events.matches("event: run_complete").count(), 1, "{events}");
    assert!(events.contains("\"status\":\"aborted\""), "{events}");
}
