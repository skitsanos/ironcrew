use super::fixture::*;
use ironcrew::llm::provider::StreamChunk;
use ironcrew::usage::{UsageCoverage, UsageTracker};
use serde_json::json;

fn partial(kind: Provider) -> String {
    match kind {
        Provider::Chat => format!(
            "data: {}\n\n",
            json!({"choices":[{"delta":{"content":"ready"}}],
            "usage":{"prompt_tokens":10,"completion_tokens":2,"total_tokens":12}})
        ),
        Provider::Responses => {
            event(
                "response.in_progress",
                json!({"response":{"usage":{
            "input_tokens":10,"output_tokens":2,"total_tokens":12}}}),
            ) + &event("response.output_text.delta", json!({"delta":"ready"}))
        }
        Provider::Anthropic => {
            event(
                "message_start",
                json!({"message":{"usage":{
            "input_tokens":10,"output_tokens":2,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}),
            ) + &event(
                "content_block_delta",
                json!({"delta":{"type":"text_delta","text":"ready"}}),
            )
        }
    }
}

fn terminal(kind: Provider) -> String {
    match kind {
        Provider::Chat => format!(
            "data: {}\n\ndata: [DONE]\n\n",
            json!({"choices":[],
            "usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15,
                "completion_tokens_details":{"reasoning_tokens":3}}})
        ),
        Provider::Responses => event(
            "response.completed",
            json!({"response":{"usage":{
            "input_tokens":10,"output_tokens":5,"total_tokens":15,
            "output_tokens_details":{"reasoning_tokens":3}}}}),
        ),
        Provider::Anthropic => {
            event(
                "message_delta",
                json!({"usage":{
            "output_tokens":5,"output_tokens_details":{"thinking_tokens":3}}}),
            ) + &event("message_stop", json!({"type":"message_stop"}))
        }
    }
}

#[test]
fn final_stream_receipts_replace_cumulative_snapshots() {
    isolated(
        "streams::final_stream_receipts_replace_cumulative_snapshots",
        async {
            for kind in [Provider::Chat, Provider::Responses, Provider::Anthropic] {
                let mut server = Server::new(
                    200,
                    "text/event-stream",
                    partial(kind) + &terminal(kind),
                    false,
                )
                .await;
                let tracker = UsageTracker::default();
                let (tx, _rx) = tokio::sync::mpsc::channel(16);
                let response = kind
                    .create(server.base.clone())
                    .chat_stream(request(&tracker), tx)
                    .await
                    .unwrap();
                assert_eq!(response.content.as_deref(), Some("ready"));
                let snapshot = tracker.snapshot().unwrap();
                assert_eq!(
                    ironcrew::usage::UsageSnapshot::from_receipt(response.usage),
                    snapshot
                );
                assert_eq!(snapshot.in_flight, 0);
                assert_eq!(snapshot.settled.requests(), 1);
                assert_eq!(snapshot.coverage, UsageCoverage::Complete);
                assert_eq!(snapshot.settled.total_tokens().known(), Some(15));
                assert_eq!(snapshot.settled.reasoning_tokens().known(), Some(3));
                let body = server.sent_request().await;
                if matches!(kind, Provider::Chat) {
                    assert_eq!(body["stream_options"]["include_usage"], true);
                }
            }
        },
    );
}

#[test]
fn cancelled_provider_futures_settle_partial_usage_once() {
    isolated(
        "streams::cancelled_provider_futures_settle_partial_usage_once",
        async {
            for kind in [Provider::Chat, Provider::Responses, Provider::Anthropic] {
                let server = Server::new(200, "text/event-stream", partial(kind), true).await;
                let tracker = UsageTracker::default();
                let input = request(&tracker);
                let provider = kind.create(server.base.clone());
                let (tx, mut rx) = tokio::sync::mpsc::channel(16);
                let worker = tokio::spawn(async move { provider.chat_stream(input, tx).await });
                // Text is sent after receipt capture. No sleep-based scheduling.
                assert!(matches!(
                    tokio::time::timeout(std::time::Duration::from_secs(3), rx.recv())
                        .await
                        .unwrap(),
                    Some(StreamChunk::Text(_))
                ));
                assert_eq!(tracker.snapshot().unwrap().in_flight, 1);
                worker.abort();
                assert!(worker.await.unwrap_err().is_cancelled());
                let snapshot = tracker.snapshot().unwrap();
                assert_eq!(snapshot.in_flight, 0);
                assert_eq!(snapshot.settled.requests(), 1);
                assert_eq!(snapshot.coverage, UsageCoverage::Partial);
                assert_eq!(snapshot.settled.total_tokens().known(), Some(12));
            }
        },
    );
}

#[test]
fn truncation_and_missing_final_usage_never_claim_complete() {
    isolated(
        "streams::truncation_and_missing_final_usage_never_claim_complete",
        async {
            for (kind, body) in [
                (Provider::Chat, partial(Provider::Chat)),
                (Provider::Responses, partial(Provider::Responses)),
                (Provider::Anthropic, partial(Provider::Anthropic)),
                (Provider::Chat, "data: [DONE]\n\n".to_owned()),
                (
                    Provider::Responses,
                    event("response.completed", json!({"response":{}})),
                ),
                (
                    Provider::Anthropic,
                    partial(Provider::Anthropic) + &event("message_stop", json!({})),
                ),
            ] {
                let server = Server::new(200, "text/event-stream", body, false).await;
                let tracker = UsageTracker::default();
                let (tx, _rx) = tokio::sync::mpsc::channel(16);
                let _result = kind
                    .create(server.base.clone())
                    .chat_stream(request(&tracker), tx)
                    .await;
                assert_ne!(
                    tracker.snapshot().unwrap().coverage,
                    UsageCoverage::Complete
                );
                assert_eq!(tracker.snapshot().unwrap().settled.requests(), 1);
            }
        },
    );
}

#[test]
fn terminal_failures_and_tool_decode_errors_retain_final_receipts() {
    isolated(
        "streams::terminal_failures_and_tool_decode_errors_retain_final_receipts",
        async {
            for terminal_kind in [
                "response.failed",
                "response.incomplete",
                "response.completed",
            ] {
                let body = event(
                    "response.output_item.added",
                    json!({"output_index":0,
                "item":{"type":"function_call","name":"fixture","id":"x"}}),
                ) + &event(
                    terminal_kind,
                    json!({"response":{"usage":{
                    "input_tokens":10,"output_tokens":5,"total_tokens":15}}}),
                );
                let server = Server::new(200, "text/event-stream", body, false).await;
                let tracker = UsageTracker::default();
                let (tx, _rx) = tokio::sync::mpsc::channel(16);
                let result = Provider::Responses
                    .create(server.base.clone())
                    .chat_stream(request(&tracker), tx)
                    .await;
                assert!(result.is_err()); // completed still has an invalid tool call
                assert_eq!(
                    tracker.snapshot().unwrap().settled.total_tokens().known(),
                    Some(15)
                );
                assert_eq!(
                    tracker.snapshot().unwrap().coverage,
                    UsageCoverage::Complete
                );
            }
        },
    );
}

#[test]
fn aggregate_overflow_fails_the_call_and_stays_visible() {
    isolated(
        "streams::aggregate_overflow_fails_the_call_and_stays_visible",
        async {
            use ironcrew::usage::{UsageCounts, UsageReceipt};
            let tracker = UsageTracker::default();
            tracker
                .start()
                .unwrap()
                .finish(UsageReceipt::from_counts(
                    UsageCounts {
                        prompt_tokens: Some(u64::MAX),
                        completion_tokens: Some(0),
                        total_tokens: Some(u64::MAX),
                        ..Default::default()
                    },
                    true,
                ))
                .unwrap();
            let server = Server::new(
                200,
                "text/event-stream",
                partial(Provider::Chat) + &terminal(Provider::Chat),
                false,
            )
            .await;
            let (tx, _rx) = tokio::sync::mpsc::channel(16);
            let result = Provider::Chat
                .create(server.base.clone())
                .chat_stream(request(&tracker), tx)
                .await;
            assert!(result.unwrap_err().to_string().contains("64-bit range"));
            assert!(tracker.snapshot().is_err());
            assert!(tracker.start().is_err());
        },
    );
}

#[test]
fn late_nonterminal_update_cannot_inherit_final_coverage() {
    isolated(
        "streams::late_nonterminal_update_cannot_inherit_final_coverage",
        async {
            let body = terminal(Provider::Chat)
                + &format!(
                    "data: {}\n\n",
                    json!({
            "choices":[{"delta":{"content":"late"}}],
            "usage":{"prompt_tokens":10,"completion_tokens":6,"total_tokens":16}})
                );
            let server = Server::new(200, "text/event-stream", body, false).await;
            let tracker = UsageTracker::default();
            let (tx, _rx) = tokio::sync::mpsc::channel(16);
            Provider::Chat
                .create(server.base.clone())
                .chat_stream(request(&tracker), tx)
                .await
                .unwrap();
            assert_eq!(tracker.snapshot().unwrap().coverage, UsageCoverage::Partial);
            assert_eq!(
                tracker.snapshot().unwrap().settled.total_tokens().known(),
                Some(16)
            );
        },
    );
}

#[test]
fn cancellation_before_a_receipt_is_unavailable_not_zero() {
    isolated(
        "streams::cancellation_before_a_receipt_is_unavailable_not_zero",
        async {
            let server = Server::new(200, "text/event-stream", String::new(), true).await;
            let tracker = UsageTracker::default();
            let (tx, _rx) = tokio::sync::mpsc::channel(16);
            let provider = Provider::Chat.create(server.base.clone());
            assert!(
                tokio::time::timeout(
                    std::time::Duration::from_millis(50),
                    provider.chat_stream(request(&tracker), tx)
                )
                .await
                .is_err()
            );
            let snapshot = tracker.snapshot().unwrap();
            assert_eq!(snapshot.in_flight, 0);
            assert_eq!(snapshot.settled.requests(), 1);
            assert_eq!(snapshot.coverage, UsageCoverage::Unavailable);
            assert_eq!(snapshot.settled.total_tokens().known(), None);
        },
    );
}
