use super::fixture::{Recorder, assert_usage};
use async_trait::async_trait;
use ironcrew::engine::agent::Agent;
use ironcrew::llm::provider::{ChatRequest, ChatResponse, LlmProvider, TokenUsage, ToolSchema};
use ironcrew::llm::scope::with_usage_tracker;
use ironcrew::usage::{UsageCoverage, UsageTracker};
use ironcrew::utils::error::{IronCrewError, Result};
use std::sync::Arc;

struct OpaqueProvider;

#[async_trait]
impl LlmProvider for OpaqueProvider {
    fn validate_request(&self, request: &ChatRequest, _: bool) -> Result<()> {
        if request.model == "invalid" {
            Err(IronCrewError::Validation("invalid fixture model".into()))
        } else {
            Ok(())
        }
    }
    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse> {
        assert!(
            request.usage_tracker.is_none(),
            "wrapper owns opaque accounting"
        );
        if request.model == "hang" {
            std::future::pending::<()>().await;
        }
        Ok(ChatResponse {
            content: Some("opaque".into()),
            usage: Some(TokenUsage {
                prompt_tokens: 10,
                completion_tokens: 3,
                total_tokens: 13,
                cached_tokens: 0,
            }),
            ..Default::default()
        })
    }
    async fn chat_with_tools(
        &self,
        request: ChatRequest,
        _: &[ToolSchema],
    ) -> Result<ChatResponse> {
        self.chat(request).await
    }
}

fn request(model: &str) -> ChatRequest {
    Agent::default().chat_request(model.into(), vec![])
}

#[tokio::test]
async fn nested_wrappers_and_explicit_request_scope_do_not_double_count() {
    let outer = UsageTracker::default();
    let inner = UsageTracker::default();
    let explicit = UsageTracker::default();
    let provider = with_usage_tracker(
        with_usage_tracker(Arc::new(Recorder::default()), inner.clone()),
        outer.clone(),
    );
    provider.chat(request("fixture")).await.unwrap();
    let mut req = request("fixture");
    req.usage_tracker = Some(explicit.clone());
    provider.chat_with_tools(req, &[]).await.unwrap();
    assert_usage(&outer, 1, UsageCoverage::Complete);
    assert_usage(&explicit, 1, UsageCoverage::Complete);
    assert_eq!(inner.snapshot().unwrap().settled.requests(), 0);
}

#[tokio::test]
async fn custom_provider_without_receipt_contract_is_unknown_not_legacy_zero() {
    let outer = UsageTracker::default();
    let inner = UsageTracker::default();
    let provider = with_usage_tracker(
        with_usage_tracker(Arc::new(OpaqueProvider), inner.clone()),
        outer.clone(),
    );
    provider.chat(request("fixture")).await.unwrap();
    provider
        .chat_with_tools(request("fixture"), &[])
        .await
        .unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::channel(4);
    provider.chat_stream(request("fixture"), tx).await.unwrap();
    assert!(rx.recv().await.is_some());
    let snapshot = outer.snapshot().unwrap();
    assert_eq!(snapshot.settled.requests(), 3);
    assert_eq!(snapshot.coverage, UsageCoverage::Unavailable);
    assert_eq!(snapshot.settled.total_tokens().known(), None);
    assert_eq!(inner.snapshot().unwrap().settled.requests(), 0);
}

#[tokio::test]
async fn custom_preflight_is_not_dispatch_and_cancellation_is_unknown_once() {
    let tracker = UsageTracker::default();
    let provider = with_usage_tracker(Arc::new(OpaqueProvider), tracker.clone());
    assert!(provider.chat(request("invalid")).await.is_err());
    assert_eq!(tracker.snapshot().unwrap().settled.requests(), 0);
    assert!(
        tokio::time::timeout(
            std::time::Duration::from_millis(20),
            provider.chat(request("hang"))
        )
        .await
        .is_err()
    );
    let snapshot = tracker.snapshot().unwrap();
    assert_eq!(snapshot.in_flight, 0);
    assert_eq!(snapshot.settled.requests(), 1);
    assert_eq!(snapshot.coverage, UsageCoverage::Unavailable);
}
