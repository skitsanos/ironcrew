use std::sync::atomic::{AtomicUsize, Ordering};

use tokio::sync::Notify;

use super::*;

fn sample_value(series: &str) -> u64 {
    let mut body = String::new();
    crate::metrics::append_prometheus(&mut body);
    body.lines()
        .find_map(|line| {
            line.strip_prefix(series)
                .and_then(|value| value.strip_prefix(' '))
                .and_then(|value| value.parse().ok())
        })
        .unwrap_or_else(|| panic!("missing metric series: {series}"))
}

struct BlockingProvider {
    entered: AtomicUsize,
    release: Notify,
}

#[async_trait]
impl LlmProvider for BlockingProvider {
    async fn chat(&self, _request: ChatRequest) -> Result<ChatResponse> {
        self.entered.fetch_add(1, Ordering::AcqRel);
        self.release.notified().await;
        unreachable!("test calls are cancelled before release")
    }

    async fn chat_with_tools(
        &self,
        request: ChatRequest,
        _tools: &[ToolSchema],
    ) -> Result<ChatResponse> {
        self.chat(request).await
    }
}

#[tokio::test]
async fn cancellation_releases_active_call_and_peak_is_monotonic() {
    let cancelled_series = "ironcrew_provider_requests_total{provider=\"other\",operation=\"chat\",outcome=\"cancelled\"}";
    let cancelled_before = sample_value(cancelled_series);
    let inner = Arc::new(BlockingProvider {
        entered: AtomicUsize::new(0),
        release: Notify::new(),
    });
    let metrics = Arc::new(ProviderCallMetrics::default());
    let provider: Arc<dyn LlmProvider> =
        Arc::new(ObservedProvider::new(inner.clone(), Arc::clone(&metrics)));
    let request = ChatRequest {
        messages: Vec::new(),
        model: "test".into(),
        temperature: None,
        max_tokens: None,
        response_format: None,
        prompt_cache_key: None,
        prompt_cache_retention: None,
        usage_tracker: None,
        reasoning_effort: None,
    };
    let first = tokio::spawn({
        let provider = Arc::clone(&provider);
        let request = request.clone();
        async move { provider.chat(request).await }
    });
    let second = tokio::spawn({
        let provider = Arc::clone(&provider);
        async move { provider.chat(request).await }
    });

    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        while inner.entered.load(Ordering::Acquire) != 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("both calls must enter the provider");
    assert_eq!(
        metrics.snapshot(),
        ProviderCallSnapshot { active: 2, peak: 2 }
    );

    first.abort();
    second.abort();
    let _ = first.await;
    let _ = second.await;
    assert_eq!(
        metrics.snapshot(),
        ProviderCallSnapshot { active: 0, peak: 2 }
    );
    assert_eq!(sample_value(cancelled_series), cancelled_before + 2);
}

struct TokenProvider;

#[async_trait]
impl LlmProvider for TokenProvider {
    fn metrics_family(&self) -> ProviderFamily {
        ProviderFamily::Anthropic
    }

    async fn chat(&self, _request: ChatRequest) -> Result<ChatResponse> {
        Ok(ChatResponse {
            usage: crate::usage::UsageReceipt::from_counts(
                crate::usage::UsageCounts {
                    prompt_tokens: Some(11),
                    completion_tokens: Some(7),
                    total_tokens: Some(18),
                    cached_tokens: Some(3),
                    ..Default::default()
                },
                true,
            ),
            ..ChatResponse::default()
        })
    }

    async fn chat_with_tools(
        &self,
        request: ChatRequest,
        _tools: &[ToolSchema],
    ) -> Result<ChatResponse> {
        self.chat(request).await
    }
}

#[tokio::test]
async fn successful_calls_record_fixed_operation_and_token_totals() {
    let requests = "ironcrew_provider_requests_total{provider=\"anthropic\",operation=\"chat_with_tools\",outcome=\"success\"}";
    let prompt_tokens = "ironcrew_provider_tokens_total{provider=\"anthropic\",type=\"prompt\"}";
    let completion_tokens =
        "ironcrew_provider_tokens_total{provider=\"anthropic\",type=\"completion\"}";
    let cached_tokens = "ironcrew_provider_tokens_total{provider=\"anthropic\",type=\"cached\"}";
    let before = [
        sample_value(requests),
        sample_value(prompt_tokens),
        sample_value(completion_tokens),
        sample_value(cached_tokens),
    ];
    let provider = ObservedProvider::new(
        Arc::new(TokenProvider),
        Arc::new(ProviderCallMetrics::default()),
    );
    let request = ChatRequest {
        messages: Vec::new(),
        model: "test".into(),
        temperature: None,
        max_tokens: None,
        response_format: None,
        prompt_cache_key: None,
        prompt_cache_retention: None,
        usage_tracker: None,
        reasoning_effort: None,
    };

    provider.chat_with_tools(request, &[]).await.unwrap();

    assert_eq!(sample_value(requests), before[0] + 1);
    assert_eq!(sample_value(prompt_tokens), before[1] + 11);
    assert_eq!(sample_value(completion_tokens), before[2] + 7);
    assert_eq!(sample_value(cached_tokens), before[3] + 3);
}

struct ErrorProvider;

#[async_trait]
impl LlmProvider for ErrorProvider {
    fn metrics_family(&self) -> ProviderFamily {
        ProviderFamily::OpenAiResponses
    }

    async fn chat(&self, _request: ChatRequest) -> Result<ChatResponse> {
        Err(crate::utils::error::IronCrewError::Provider(
            "private-provider-error-body".into(),
        ))
    }

    async fn chat_with_tools(
        &self,
        request: ChatRequest,
        _tools: &[ToolSchema],
    ) -> Result<ChatResponse> {
        self.chat(request).await
    }
}

#[tokio::test]
async fn errors_record_only_the_fixed_outcome() {
    let errors = "ironcrew_provider_requests_total{provider=\"openai_responses\",operation=\"chat\",outcome=\"error\"}";
    let before = sample_value(errors);
    let provider = ObservedProvider::new(
        Arc::new(ErrorProvider),
        Arc::new(ProviderCallMetrics::default()),
    );
    let request = ChatRequest {
        messages: Vec::new(),
        model: "private-model-name".into(),
        temperature: None,
        max_tokens: None,
        response_format: None,
        prompt_cache_key: None,
        prompt_cache_retention: None,
        usage_tracker: None,
        reasoning_effort: None,
    };

    provider.chat(request).await.expect_err("provider fails");

    assert_eq!(sample_value(errors), before + 1);
    let mut body = String::new();
    crate::metrics::append_prometheus(&mut body);
    assert!(!body.contains("private-provider-error-body"));
    assert!(!body.contains("private-model-name"));
}
