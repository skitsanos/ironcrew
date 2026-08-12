use super::ic017_http::{
    assert_error_cache_policy, assert_exact_ids, assert_no_forbidden, assert_sse_cache_policy,
    event_url,
};
use super::ic017_support::{
    lock_journal_reads, parse_sse_frames, read_until_sse_event, unlock_journal_reads,
};
use super::*;

pub(super) async fn assert_read_deadline_contract(pair: &mut ProcessPair, run_id: &str) {
    let mut stream = authenticated(pair.client.get(event_url(pair, run_id)))
        .send()
        .await
        .expect("open IC-017 timeout stream through replica B");
    assert_eq!(stream.status(), StatusCode::OK);
    assert_sse_cache_policy(&stream);

    let barrier = lock_journal_reads(&pair.database_url, &pair.prefix, run_id).await;
    let unavailable = tokio::time::timeout(
        Duration::from_secs(2),
        authenticated(pair.client.get(event_url(pair, run_id))).send(),
    )
    .await
    .expect("initial IC-017 read exceeded its HTTP deadline")
    .expect("read initial IC-017 timeout response");
    assert_eq!(unavailable.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_error_cache_policy(&unavailable);
    let unavailable = unavailable
        .text()
        .await
        .expect("read initial IC-017 timeout body");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&unavailable).unwrap(),
        serde_json::json!({"error": "Run-event replay is temporarily unavailable"})
    );
    assert_no_forbidden("initial read timeout", &unavailable);

    let body = tokio::time::timeout(
        Duration::from_secs(8),
        read_until_sse_event(&mut stream, "error"),
    )
    .await
    .expect("five IC-017 stream read deadlines were not bounded");
    unlock_journal_reads(barrier).await;
    let frames = parse_sse_frames(&body);
    assert_eq!(frames.len(), 2, "{body}");
    assert_exact_ids(&frames[..1], run_id, &[1]);
    assert_eq!(frames[0].event, "human_input_requested");
    assert_eq!(frames[0].data["event"], "human_input_requested");
    assert_eq!(
        frames[0].data["data"]["question_metadata"],
        "omitted_from_event_journal"
    );
    assert_eq!(frames[1].id, None);
    assert_eq!(frames[1].event, "error");
    assert_eq!(
        frames[1].data,
        serde_json::json!({
            "event": "error",
            "data": {
                "message": "run-event replay timed out; reconnect with Last-Event-ID"
            }
        })
    );
    assert_no_forbidden("read timeout stream", &body);
    let ended = tokio::time::timeout(Duration::from_secs(2), stream.chunk())
        .await
        .expect("timed out waiting for IC-017 timeout stream to close")
        .expect("read IC-017 timeout stream close");
    assert!(ended.is_none(), "timeout stream must close after its error");

    wait_ready(pair).await;
    let mut recovered = authenticated(pair.client.get(event_url(pair, run_id)))
        .send()
        .await
        .expect("reconnect IC-017 stream after read timeout");
    assert_eq!(recovered.status(), StatusCode::OK);
    assert_sse_cache_policy(&recovered);
    let recovered_body = read_until_sse_event(&mut recovered, "human_input_requested").await;
    let recovered_frames = parse_sse_frames(&recovered_body);
    assert_eq!(recovered_frames.len(), 1, "{recovered_body}");
    assert_exact_ids(&recovered_frames, run_id, &[1]);
    assert_no_forbidden("recovered IC-017 stream", &recovered_body);
}
