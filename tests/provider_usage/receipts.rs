use super::fixture::*;
use ironcrew::usage::{UsageCoverage, UsageTracker};
use serde_json::json;

#[test]
fn tool_requests_preserve_known_zero_usage() {
    isolated("receipts::tool_requests_preserve_known_zero_usage", async {
        for (kind, response) in [
            (
                Provider::Chat,
                json!({"choices":[],"usage":{"prompt_tokens":0,"completion_tokens":0,"total_tokens":0}}),
            ),
            (
                Provider::Responses,
                json!({"status":"completed","output":[],"usage":{"input_tokens":0,"output_tokens":0,"total_tokens":0}}),
            ),
            (
                Provider::Anthropic,
                json!({"content":[],"usage":{"input_tokens":0,"output_tokens":0,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}),
            ),
        ] {
            let tracker = UsageTracker::default();
            let server = Server::new(200, "application/json", response.to_string(), false).await;
            kind.create(server.base.clone())
                .chat_with_tools(request(&tracker), &[])
                .await
                .unwrap();
            let snapshot = tracker.snapshot().unwrap();
            assert_eq!(snapshot.settled.requests(), 1);
            assert_eq!(snapshot.coverage, UsageCoverage::Complete);
            assert_eq!(snapshot.settled.total_tokens().known(), Some(0));
        }
    });
}

#[test]
fn nonstreaming_receipts_preserve_large_counts_and_detail() {
    isolated(
        "receipts::nonstreaming_receipts_preserve_large_counts_and_detail",
        async {
            for (kind, response) in [
                (
                    Provider::Chat,
                    json!({"choices":[{"message":{"content":"ok"}}],
                "usage":{"prompt_tokens":5_000_000_000_u64,"completion_tokens":20,
                    "total_tokens":5_000_000_020_u64,"prompt_tokens_details":{"cached_tokens":4,"cache_write_tokens":6},
                    "completion_tokens_details":{"reasoning_tokens":12}}}),
                ),
                (
                    Provider::Responses,
                    json!({"status":"completed","output":[],
                "usage":{"input_tokens":5_000_000_000_u64,"output_tokens":20,
                    "total_tokens":5_000_000_020_u64,"input_tokens_details":{"cached_tokens":4,"cache_write_tokens":6},
                    "output_tokens_details":{"reasoning_tokens":12}}}),
                ),
                (
                    Provider::Anthropic,
                    json!({"content":[],"usage":{
                "input_tokens":4_999_999_990_u64,"output_tokens":20,
                "cache_read_input_tokens":4,"cache_creation_input_tokens":6,
                "output_tokens_details":{"thinking_tokens":12}}}),
                ),
            ] {
                let mut server =
                    Server::new(200, "application/json", response.to_string(), false).await;
                let tracker = UsageTracker::default();
                let provider = kind.create(server.base.clone());
                provider.chat(request(&tracker)).await.unwrap();
                let usage = tracker.snapshot().unwrap();
                assert_eq!(usage.in_flight, 0);
                assert_eq!(usage.settled.requests(), 1);
                assert_eq!(usage.coverage, UsageCoverage::Complete);
                assert_eq!(usage.settled.total_tokens().known(), Some(5_000_000_020));
                assert_eq!(usage.settled.reasoning_tokens().known(), Some(12));
                assert_eq!(usage.settled.cached_tokens().known(), Some(4));
                assert_eq!(usage.settled.cache_write_tokens().known(), Some(6));
                assert!(server.sent_request().await.get("usage_tracker").is_none());
            }
        },
    );
}

#[test]
fn decoding_failure_and_retry_keep_both_receipts() {
    isolated(
        "receipts::decoding_failure_and_retry_keep_both_receipts",
        async {
            let tracker = UsageTracker::default();
            for (index, output) in [json!(null), json!([])].into_iter().enumerate() {
                let response = json!({"status":"completed","output":output,"usage":{
                "input_tokens":10,"output_tokens":5,"total_tokens":15}});
                let server =
                    Server::new(200, "application/json", response.to_string(), false).await;
                let result = Provider::Responses
                    .create(server.base.clone())
                    .chat(request(&tracker))
                    .await;
                assert_eq!(result.is_ok(), index == 1);
            }
            let snapshot = tracker.snapshot().unwrap();
            assert_eq!(snapshot.settled.requests(), 2);
            assert_eq!(snapshot.settled.total_tokens().known(), Some(30));
            assert_eq!(snapshot.coverage, UsageCoverage::Complete);
        },
    );
}

#[test]
fn http_errors_missing_and_malformed_receipts_remain_truthful() {
    isolated(
        "receipts::http_errors_missing_and_malformed_receipts_remain_truthful",
        async {
            let tracker = UsageTracker::default();
            for (status, response, coverage) in [
                (
                    500,
                    json!({"error":{"message":"fixture"},"usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}}),
                    UsageCoverage::Complete,
                ),
                (200, json!({"choices":[]}), UsageCoverage::Unavailable),
                (
                    200,
                    json!({"choices":[],"usage":{"prompt_tokens":"bad","completion_tokens":5}}),
                    UsageCoverage::Partial,
                ),
            ] {
                let local = UsageTracker::default();
                let server =
                    Server::new(status, "application/json", response.to_string(), false).await;
                let result = Provider::Chat
                    .create(server.base.clone())
                    .chat(request(&local))
                    .await;
                assert_eq!(result.is_ok(), status == 200);
                assert_eq!(local.snapshot().unwrap().coverage, coverage);
                assert_eq!(local.snapshot().unwrap().settled.requests(), 1);
            }
            // Local validation must not invent a dispatched request.
            let provider = ironcrew::llm::openai::OpenAiProvider::new(String::new(), None);
            use ironcrew::llm::provider::LlmProvider;
            assert!(provider.chat(request(&tracker)).await.is_err());
            assert_eq!(tracker.snapshot().unwrap().settled.requests(), 0);
        },
    );
}

#[test]
fn parallel_requests_share_only_the_explicit_scope() {
    isolated(
        "receipts::parallel_requests_share_only_the_explicit_scope",
        async {
            let tracker = UsageTracker::default();
            let other = UsageTracker::default();
            let mut workers = tokio::task::JoinSet::new();
            for _ in 0..8 {
                let tracker = tracker.clone();
                workers.spawn(async move {
                    let server = Server::new(
                        200,
                        "application/json",
                        json!({"choices":[],
                    "usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}})
                        .to_string(),
                        false,
                    )
                    .await;
                    Provider::Chat
                        .create(server.base.clone())
                        .chat(request(&tracker))
                        .await
                        .unwrap();
                });
            }
            while let Some(result) = workers.join_next().await {
                result.unwrap();
            }
            assert_eq!(tracker.snapshot().unwrap().settled.requests(), 8);
            assert_eq!(
                tracker.snapshot().unwrap().settled.total_tokens().known(),
                Some(120)
            );
            assert_eq!(other.snapshot().unwrap().settled.requests(), 0);
        },
    );
}

#[test]
fn unfinished_responses_receipt_keeps_only_partial_coverage() {
    isolated(
        "receipts::unfinished_responses_receipt_keeps_only_partial_coverage",
        async {
            for status in [json!(null), json!("queued"), json!("in_progress")] {
                let response = json!({"status":status,"output":[],"usage":{
                "input_tokens":10,"output_tokens":2,"total_tokens":12}});
                let server =
                    Server::new(200, "application/json", response.to_string(), false).await;
                let tracker = UsageTracker::default();
                Provider::Responses
                    .create(server.base.clone())
                    .chat(request(&tracker))
                    .await
                    .unwrap();
                assert_eq!(
                    tracker.snapshot().unwrap().settled.total_tokens().known(),
                    Some(12)
                );
                assert_eq!(tracker.snapshot().unwrap().coverage, UsageCoverage::Partial);
            }
        },
    );
}
