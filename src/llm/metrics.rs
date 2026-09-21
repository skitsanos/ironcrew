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
    record_usage: bool,
}

impl ProviderObservation {
    fn start(family: ProviderFamily, operation: ProviderOperation, record_usage: bool) -> Self {
        Self {
            family,
            operation,
            started_at: Instant::now(),
            completed: false,
            record_usage,
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
        if self.record_usage {
            let unavailable = crate::usage::UsageReceipt::default();
            let usage = result
                .as_ref()
                .map_or(&unavailable, |response| &response.usage);
            crate::metrics::record_provider_usage(self.family, usage);
        }
        self.completed = true;
    }
}

impl Drop for ProviderObservation {
    fn drop(&mut self) {
        if !self.completed {
            if self.record_usage {
                crate::metrics::record_provider_usage(self.family, &Default::default());
            }
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
    fn supports_token_budget(&self) -> bool {
        self.inner.supports_token_budget()
    }
    fn records_usage_metrics(&self) -> bool {
        true
    }
    fn usage_tracker(&self) -> Option<crate::usage::UsageTracker> {
        self.inner.usage_tracker()
    }
    fn records_usage(&self) -> bool {
        self.inner.records_usage()
    }

    fn validate_request(&self, request: &ChatRequest, has_tools: bool) -> Result<()> {
        self.inner.validate_request(request, has_tools)
    }

    fn metrics_family(&self) -> ProviderFamily {
        self.family
    }

    fn execution_fingerprint(&self) -> Result<String> {
        self.inner.execution_fingerprint()
    }

    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse> {
        let _guard = self.metrics.enter();
        let observation = ProviderObservation::start(
            self.family,
            ProviderOperation::Chat,
            !self.inner.records_usage_metrics(),
        );
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
        let observation = ProviderObservation::start(
            self.family,
            ProviderOperation::ChatWithTools,
            !self.inner.records_usage_metrics(),
        );
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
        let observation = ProviderObservation::start(
            self.family,
            ProviderOperation::ChatStream,
            !self.inner.records_usage_metrics(),
        );
        let result = self.inner.chat_stream(request, tx).await;
        observation.finish(&result);
        result
    }
}

#[cfg(test)]
mod tests;
