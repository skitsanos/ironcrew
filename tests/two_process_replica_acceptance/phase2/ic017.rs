use reqwest::header::HeaderValue;

use super::ic017_deadline::assert_read_deadline_contract;
use super::ic017_http::*;
use super::ic017_support::*;
use super::*;

const IC017_KEY: &str = "ic017-process-cursor-key-0001";
const JOURNAL_ENV: &[(&str, &str)] = &[
    ("IRONCREW_MAX_EVENTS", "4"),
    ("IRONCREW_EVENT_MAX_BYTES", "1024"),
    ("IRONCREW_EVENT_JOURNAL_PAGE_MAX_BYTES", "1024"),
    ("IRONCREW_EVENT_JOURNAL_READ_TIMEOUT_MS", "100"),
    ("IRONCREW_EVENT_JOURNAL_MAX_TOTAL_EVENTS", "64"),
    ("IRONCREW_EVENT_JOURNAL_PRUNE_BATCH", "4"),
];

fn assert_bounded_midrun(snapshot: &JournalSnapshot) {
    assert_eq!(snapshot.latest_sequence, 4);
    assert_eq!(snapshot.dropped_through, 0);
    assert_eq!(snapshot.retained_events, 4);
    assert_eq!(snapshot.retained_bytes, 4096);
    assert_eq!(snapshot.global_events, 4);
    assert_eq!(snapshot.global_bytes, 4096);
    assert!(snapshot.journal_complete);
    assert_eq!(snapshot.eviction_reason, None);
    assert_eq!(snapshot.terminal_event_sequence, None);
    assert_eq!(
        snapshot
            .rows
            .iter()
            .map(|row| (row.sequence, row.event_type.as_str()))
            .collect::<Vec<_>>(),
        vec![
            (1, "human_input_requested"),
            (2, "human_input_received"),
            (3, "log"),
            (4, "human_input_requested"),
        ]
    );
    for row in &snapshot.rows {
        assert!(row.payload_bytes <= 1024);
        assert_eq!(row.accounted_bytes, 1024);
        assert_no_forbidden("mid-run durable journal row", &row.payload.to_string());
    }
}

#[tokio::test]
async fn ic017_durable_sse_edges_cross_real_process_boundary() {
    with_configured_process_pair(
        "017",
        true,
        include_str!("../../fixtures/two_process_replica/crew.lua"),
        JOURNAL_ENV,
        |pair| {
            Box::pin(async move {
                let pool = sqlx::PgPool::connect(&pair.database_url)
                    .await
                    .expect("connect IC-017 observer");
                let started = start_keyed_run(pair, IC017_KEY).await;
                let run_id = started["run_id"].as_str().expect("IC-017 run id");
                assert_eq!(started["owner_instance_id"], pair.owner_a_id);

                let first_question = wait_for_shared_question(pair, run_id, FIRST_PROMPT).await;
                let first_question_id = first_question["question_id"]
                    .as_str()
                    .expect("first IC-017 question id");
                let initial = wait_for_journal(
                    &pool,
                    &pair.prefix,
                    run_id,
                    "the first durable event",
                    |snapshot| snapshot.latest_sequence == 1 && snapshot.rows.len() == 1,
                )
                .await;
                assert_eq!(initial.dropped_through, 0);
                assert_eq!(initial.retained_events, 1);
                assert_eq!(initial.global_events, 1);
                assert_eq!(initial.rows[0].sequence, 1);
                assert_eq!(initial.rows[0].event_type, "human_input_requested");

                assert_initial_http_edges(pair, run_id).await;
                assert_read_deadline_contract(pair, run_id).await;

                let (status, body) = answer_question(
                    &pair.client,
                    &pair.survivor_b.base_url,
                    run_id,
                    first_question_id,
                    FIRST_ANSWER,
                )
                .await;
                assert_eq!(status, StatusCode::ACCEPTED, "{body}");
                assert_no_forbidden("first answer response", &body);
                let second_question = wait_for_shared_question(pair, run_id, SECOND_PROMPT).await;
                let second_question_id = second_question["question_id"]
                    .as_str()
                    .expect("second IC-017 question id");
                let midrun = wait_for_journal(
                    &pool,
                    &pair.prefix,
                    run_id,
                    "the complete four-event mid-run window",
                    |snapshot| snapshot.latest_sequence == 4 && snapshot.rows.len() == 4,
                )
                .await;
                assert_bounded_midrun(&midrun);
                let (status, body) = answer_question(
                    &pair.client,
                    &pair.survivor_b.base_url,
                    run_id,
                    second_question_id,
                    SECOND_ANSWER,
                )
                .await;
                assert_eq!(status, StatusCode::ACCEPTED, "{body}");
                assert_no_forbidden("second answer response", &body);
                assert_eq!(
                    wait_for_status(pair, run_id, "Success").await["status"],
                    "Success"
                );

                let bounded = wait_for_journal(
                    &pool,
                    &pair.prefix,
                    run_id,
                    "the four-row terminal window",
                    |snapshot| {
                        snapshot.latest_sequence == 7
                            && snapshot.dropped_through == 3
                            && snapshot.rows.len() == 4
                            && snapshot.terminal_event_sequence == Some(7)
                    },
                )
                .await;
                assert_bounded_terminal(&bounded);

                assert_cursor_error(
                    pair,
                    run_id,
                    HeaderValue::from_str(&format!("{run_id}:1")).unwrap(),
                    StatusCode::CONFLICT,
                    "cursor_expired",
                    "run-event cursor has expired (events through 3 were pruned)",
                )
                .await;

                let terminal = authenticated(pair.client.get(event_url(pair, run_id)))
                    .send()
                    .await
                    .expect("read bounded terminal replay through replica B");
                assert_eq!(terminal.status(), StatusCode::OK);
                assert_sse_cache_policy(&terminal);
                let terminal_body = terminal.text().await.expect("read bounded terminal SSE");
                let terminal_frames = parse_sse_frames(&terminal_body);
                assert_eq!(terminal_frames.len(), 5, "{terminal_body}");
                assert_exact_ids(&terminal_frames, run_id, &[3, 4, 5, 6, 7]);
                assert_eq!(
                    terminal_frames
                        .iter()
                        .map(|frame| frame.event.as_str())
                        .collect::<Vec<_>>(),
                    vec![
                        "journal_gap",
                        "human_input_requested",
                        "human_input_received",
                        "log",
                        "run_complete",
                    ]
                );
                assert_eq!(
                    terminal_frames[0].data,
                    serde_json::json!({
                        "event": "journal_gap",
                        "data": {
                            "first_sequence": 1,
                            "last_sequence": 3,
                            "reason": "writer_backpressure"
                        }
                    })
                );
                assert_no_forbidden("bounded terminal replay", &terminal_body);

                let resumed = authenticated(pair.client.get(event_url(pair, run_id)))
                    .header("Last-Event-ID", format!("{run_id}:3"))
                    .send()
                    .await
                    .expect("resume bounded terminal replay through replica B");
                assert_eq!(resumed.status(), StatusCode::OK);
                assert_sse_cache_policy(&resumed);
                let resumed_body = resumed.text().await.expect("read resumed IC-017 SSE");
                let resumed_frames = parse_sse_frames(&resumed_body);
                assert_eq!(resumed_frames.len(), 4, "{resumed_body}");
                assert_exact_ids(&resumed_frames, run_id, &[4, 5, 6, 7]);
                assert!(
                    resumed_frames
                        .iter()
                        .all(|frame| frame.event != "journal_gap")
                );
                assert_no_forbidden("resumed terminal replay", &resumed_body);

                expire_journal_rows(&pool, &pair.prefix, run_id, 4).await;
                let fallback = authenticated(pair.client.get(event_url(pair, run_id)))
                    .send()
                    .await
                    .expect("read retention fallback through replica B");
                assert_eq!(fallback.status(), StatusCode::OK);
                assert_sse_cache_policy(&fallback);
                let fallback_body = fallback.text().await.expect("read IC-017 fallback SSE");
                let fallback_frames = parse_sse_frames(&fallback_body);
                assert_eq!(fallback_frames.len(), 2, "{fallback_body}");
                assert_eq!(fallback_frames[0].id, Some(format!("{run_id}:7")));
                assert_eq!(fallback_frames[0].event, "journal_gap");
                assert_eq!(fallback_frames[0].data["data"]["first_sequence"], 1);
                assert_eq!(fallback_frames[0].data["data"]["last_sequence"], 7);
                assert_eq!(fallback_frames[0].data["data"]["reason"], "retention");
                assert_eq!(fallback_frames[1].id, None);
                assert_eq!(fallback_frames[1].event, "run_complete");
                assert_eq!(fallback_frames[1].data["data"]["status"], "success");
                assert_eq!(fallback_frames[1].data["data"]["journal_complete"], false);
                assert_eq!(
                    fallback_frames[1].data["data"]["synthesized_from_run_record"],
                    true
                );
                assert_no_forbidden("retention fallback", &fallback_body);

                let repeated = authenticated(pair.client.get(event_url(pair, run_id)))
                    .send()
                    .await
                    .expect("repeat retention fallback through replica B");
                assert_eq!(repeated.status(), StatusCode::OK);
                assert_sse_cache_policy(&repeated);
                let repeated = repeated
                    .text()
                    .await
                    .expect("read repeated IC-017 fallback");
                assert_eq!(parse_sse_frames(&repeated), fallback_frames);

                let pruned = wait_for_journal(
                    &pool,
                    &pair.prefix,
                    run_id,
                    "physical retention cleanup",
                    |snapshot| snapshot.rows.is_empty() && snapshot.retained_events == 0,
                )
                .await;
                assert_eq!(pruned.latest_sequence, 7);
                assert_eq!(pruned.dropped_through, 7);
                assert_eq!(pruned.retained_bytes, 0);
                assert_eq!(pruned.global_events, 0);
                assert_eq!(pruned.global_bytes, 0);
                assert_eq!(pruned.eviction_reason.as_deref(), Some("retention"));
                assert_eq!(pruned.terminal_event_sequence, Some(7));
                assert!(pruned.journal_complete);
                pool.close().await;
            })
        },
    )
    .await;
}
