use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};

use async_trait::async_trait;

use super::provider::{ChatRequest, ChatResponse, LlmProvider, StreamChunk, ToolSchema};
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
}

impl ObservedProvider {
    fn new(inner: Arc<dyn LlmProvider>, metrics: Arc<ProviderCallMetrics>) -> Self {
        Self { inner, metrics }
    }
}

#[async_trait]
impl LlmProvider for ObservedProvider {
    fn execution_fingerprint(&self) -> Result<String> {
        self.inner.execution_fingerprint()
    }

    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse> {
        let _guard = self.metrics.enter();
        self.inner.chat(request).await
    }

    async fn chat_with_tools(
        &self,
        request: ChatRequest,
        tools: &[ToolSchema],
    ) -> Result<ChatResponse> {
        let _guard = self.metrics.enter();
        self.inner.chat_with_tools(request, tools).await
    }

    async fn chat_stream(
        &self,
        request: ChatRequest,
        tx: tokio::sync::mpsc::Sender<StreamChunk>,
    ) -> Result<ChatResponse> {
        let _guard = self.metrics.enter();
        self.inner.chat_stream(request, tx).await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use tokio::sync::Notify;

    use super::*;

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
    }
}
