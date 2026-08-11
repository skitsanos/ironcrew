use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Instant;

use async_trait::async_trait;

use super::provider::{ChatRequest, ChatResponse, LlmProvider, StreamChunk, ToolSchema};
use crate::metrics::{ProviderFamily, ProviderOperation, ProviderOutcome};
use crate::utils::error::Result;

#[derive(Debug, Default)]
struct ProviderCallMetrics {
    active: AtomicUsize,
    peak: AtomicUsize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ProviderCallSnapshot {
    pub active: usize,
    pub peak: usize,
}

static GLOBAL_METRICS: OnceLock<Arc<ProviderCallMetrics>> = OnceLock::new();

fn global_metrics() -> Arc<ProviderCallMetrics> {
    Arc::clone(GLOBAL_METRICS.get_or_init(|| Arc::new(ProviderCallMetrics::default())))
}

pub(crate) fn provider_call_snapshot() -> ProviderCallSnapshot {
    global_metrics().snapshot()
}

pub(crate) fn observe_provider(provider: Arc<dyn LlmProvider>) -> Arc<dyn LlmProvider> {
    Arc::new(ObservedProvider::new(provider, global_metrics()))
}

pub(crate) fn observe_boxed_provider(provider: Box<dyn LlmProvider>) -> Arc<dyn LlmProvider> {
    observe_provider(Arc::from(provider))
}

impl ProviderCallMetrics {
    fn enter(&self) -> ProviderCallGuard<'_> {
        let mut observed = self.active.load(Ordering::Acquire);
        let active = loop {
            let Some(next) = observed.checked_add(1) else {
                return ProviderCallGuard {
                    metrics: self,
                    counted: false,
                };
            };
            match self.active.compare_exchange_weak(
                observed,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break next,
                Err(current) => observed = current,
            }
        };
        self.peak.fetch_max(active, Ordering::AcqRel);
        ProviderCallGuard {
            metrics: self,
            counted: true,
        }
    }

    fn snapshot(&self) -> ProviderCallSnapshot {
        ProviderCallSnapshot {
            active: self.active.load(Ordering::Acquire),
            peak: self.peak.load(Ordering::Acquire),
        }
    }
}

struct ProviderCallGuard<'a> {
    metrics: &'a ProviderCallMetrics,
    counted: bool,
}

impl Drop for ProviderCallGuard<'_> {
    fn drop(&mut self) {
        if self.counted {
            self.metrics.active.fetch_sub(1, Ordering::AcqRel);
        }
    }
}

struct ObservedProvider {
    inner: Arc<dyn LlmProvider>,
    metrics: Arc<ProviderCallMetrics>,
    family: ProviderFamily,
}

impl ObservedProvider {
    fn new(inner: Arc<dyn LlmProvider>, metrics: Arc<ProviderCallMetrics>) -> Self {
        let family = inner.metrics_family();
        Self {
            inner,
            metrics,
            family,
        }
    }
}

struct ProviderObservation {
    family: ProviderFamily,
    operation: ProviderOperation,
    started_at: Instant,
    completed: bool,
}

impl ProviderObservation {
    fn start(family: ProviderFamily, operation: ProviderOperation) -> Self {
        Self {
            family,
            operation,
            started_at: Instant::now(),
            completed: false,
        }
    }

    fn finish(mut self, result: &Result<ChatResponse>) {
        let outcome = if result.is_ok() {
            ProviderOutcome::Success
        } else {
            ProviderOutcome::Error
        };
        crate::metrics::record_provider(
            self.family,
            self.operation,
            outcome,
            self.started_at.elapsed(),
        );
        if let Ok(response) = result
            && let Some(usage) = &response.usage
        {
            crate::metrics::record_provider_tokens(self.family, usage);
        }
        self.completed = true;
    }
}

impl Drop for ProviderObservation {
    fn drop(&mut self) {
        if !self.completed {
            crate::metrics::record_provider(
                self.family,
                self.operation,
                ProviderOutcome::Cancelled,
                self.started_at.elapsed(),
            );
        }
    }
}

#[async_trait]
impl LlmProvider for ObservedProvider {
    fn metrics_family(&self) -> ProviderFamily {
        self.family
    }

    fn execution_fingerprint(&self) -> Result<String> {
        self.inner.execution_fingerprint()
    }

    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse> {
        let _guard = self.metrics.enter();
        let observation = ProviderObservation::start(self.family, ProviderOperation::Chat);
        let result = self.inner.chat(request).await;
        observation.finish(&result);
        result
    }

    async fn chat_with_tools(
        &self,
        request: ChatRequest,
        tools: &[ToolSchema],
    ) -> Result<ChatResponse> {
        let _guard = self.metrics.enter();
        let observation = ProviderObservation::start(self.family, ProviderOperation::ChatWithTools);
        let result = self.inner.chat_with_tools(request, tools).await;
        observation.finish(&result);
        result
    }

    async fn chat_stream(
        &self,
        request: ChatRequest,
        tx: tokio::sync::mpsc::Sender<StreamChunk>,
    ) -> Result<ChatResponse> {
        let _guard = self.metrics.enter();
        let observation = ProviderObservation::start(self.family, ProviderOperation::ChatStream);
        let result = self.inner.chat_stream(request, tx).await;
        observation.finish(&result);
        result
    }
}

#[cfg(test)]
mod tests {
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
                usage: Some(super::super::provider::TokenUsage {
                    prompt_tokens: 11,
                    completion_tokens: 7,
                    total_tokens: 18,
                    cached_tokens: 3,
                }),
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
        let prompt_tokens =
            "ironcrew_provider_tokens_total{provider=\"anthropic\",type=\"prompt\"}";
        let completion_tokens =
            "ironcrew_provider_tokens_total{provider=\"anthropic\",type=\"completion\"}";
        let cached_tokens =
            "ironcrew_provider_tokens_total{provider=\"anthropic\",type=\"cached\"}";
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
        };

        provider.chat(request).await.expect_err("provider fails");

        assert_eq!(sample_value(errors), before + 1);
        let mut body = String::new();
        crate::metrics::append_prometheus(&mut body);
        assert!(!body.contains("private-provider-error-body"));
        assert!(!body.contains("private-model-name"));
    }
}
