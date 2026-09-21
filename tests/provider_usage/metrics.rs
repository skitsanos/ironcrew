use super::fixture::*;
use ironcrew::llm::provider::StreamChunk;
use ironcrew::usage::UsageTracker;
use serde_json::json;

fn value(series: &str) -> u64 {
    let mut body = String::new();
    ironcrew::metrics::append_prometheus(&mut body);
    body.lines()
        .find_map(|line| {
            let (name, value) = line.rsplit_once(' ')?;
            (name == series).then(|| value.parse().unwrap())
        })
        .unwrap()
}

#[test]
fn transport_metrics_include_unscoped_success_failure_and_cancellation_once() {
    isolated(
        "metrics::transport_metrics_include_unscoped_success_failure_and_cancellation_once",
        async {
            let receipt = json!({"prompt_tokens":5_000_000_000_u64, "completion_tokens":7,
            "total_tokens":5_000_000_007_u64,"completion_tokens_details":{"reasoning_tokens":4}});
            for status in [200, 500] {
                let server = Server::new(
                    status,
                    "application/json",
                    json!({
                        "choices":[], "error":{"message":"fixture"}, "usage":receipt
                    })
                    .to_string(),
                    false,
                )
                .await;
                let mut input = request(&UsageTracker::default());
                input.usage_tracker = None;
                // The runtime metrics wrapper must not record the transport's
                // receipt a second time, including error paths.
                let runtime = ironcrew::engine::runtime::Runtime::new(
                    Provider::Chat.create(server.base.clone()),
                    None,
                );
                let result = runtime.provider.chat(input).await;
                assert_eq!(result.is_ok(), status == 200);
                if let Ok(response) = result {
                    assert_eq!(response.usage.counts().total_tokens, Some(5_000_000_007));
                }
            }
            let server = Server::new(
                200,
                "text/event-stream",
                format!(
                    "data: {}\n\n",
                    json!({
                        "choices":[{"delta":{"content":"ready"}}],
                        "usage":{"prompt_tokens":10,"completion_tokens":2,"total_tokens":12}
                    })
                ),
                true,
            )
            .await;
            let provider = Provider::Chat.create(server.base.clone());
            let mut input = request(&UsageTracker::default());
            input.usage_tracker = None;
            let (tx, mut rx) = tokio::sync::mpsc::channel(8);
            let worker = tokio::spawn(async move { provider.chat_stream(input, tx).await });
            assert!(matches!(
                tokio::time::timeout(std::time::Duration::from_secs(3), rx.recv())
                    .await
                    .unwrap(),
                Some(StreamChunk::Text(_))
            ));
            worker.abort();
            assert!(worker.await.unwrap_err().is_cancelled());
            assert_eq!(
                value(
                    "ironcrew_provider_usage_receipts_total{provider=\"openai\",coverage=\"complete\"}"
                ),
                2
            );
            assert_eq!(
                value(
                    "ironcrew_provider_usage_receipts_total{provider=\"openai\",coverage=\"partial\"}"
                ),
                1
            );
            assert_eq!(
                value("ironcrew_provider_tokens_total{provider=\"openai\",type=\"total\"}"),
                10_000_000_026
            );
            assert_eq!(
                value("ironcrew_provider_tokens_total{provider=\"openai\",type=\"reasoning\"}"),
                8
            );
            assert_eq!(
                value(
                    "ironcrew_provider_usage_incomplete_fields_total{provider=\"openai\",type=\"reasoning\"}"
                ),
                1
            );
            assert_eq!(
                value(
                    "ironcrew_provider_usage_incomplete_fields_total{provider=\"openai\",type=\"cached\"}"
                ),
                3
            );
        },
    );
}
