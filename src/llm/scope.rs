//! Explicit execution ownership, independent of provider credentials and runtime reuse.

use super::provider::{ChatRequest, ChatResponse, LlmProvider, StreamChunk, ToolSchema};
use crate::usage::{UsageAttempt, UsageReceipt, UsageTracker};
use crate::utils::error::{IronCrewError, Result};
use async_trait::async_trait;
use std::sync::Arc;

/// Bind calls to a caller-owned scope. Clones share accounting, not a global
/// counter. An explicitly scoped request takes precedence through nested wrappers.
pub fn with_usage_tracker(
    provider: Arc<dyn LlmProvider>,
    tracker: UsageTracker,
) -> Arc<dyn LlmProvider> {
    Arc::new(ScopedProvider {
        inner: ProviderRef::Owned(provider),
        tracker,
    })
}

pub(crate) fn ensure_scope(provider: Arc<dyn LlmProvider>) -> Arc<dyn LlmProvider> {
    if provider.records_usage() && provider.usage_tracker().is_some() {
        provider
    } else {
        let tracker = provider.usage_tracker().unwrap_or_default();
        with_usage_tracker(provider, tracker)
    }
}

pub(crate) fn tool_context(provider: &dyn LlmProvider) -> crate::tools::ToolCallContext {
    crate::tools::ToolCallContext {
        usage_tracker: provider.usage_tracker(),
        ..Default::default()
    }
}

pub(crate) fn borrow_with_usage_tracker(
    provider: &dyn LlmProvider,
    tracker: UsageTracker,
) -> impl LlmProvider + '_ {
    ScopedProvider {
        inner: ProviderRef::Borrowed(provider),
        tracker,
    }
}

pub(crate) fn child_scope(provider: &dyn LlmProvider) -> Result<UsageTracker> {
    match provider.usage_tracker() {
        Some(parent) => parent
            .child()
            .map_err(|error| IronCrewError::Provider(error.to_string())),
        None => Ok(UsageTracker::default()),
    }
}

pub(crate) fn snapshot(tracker: &UsageTracker) -> Result<crate::usage::UsageSnapshot> {
    tracker.snapshot().map_err(accounting_error)
}

enum ProviderRef<'a> {
    Owned(Arc<dyn LlmProvider>),
    Borrowed(&'a dyn LlmProvider),
}

impl<'a> std::ops::Deref for ProviderRef<'a> {
    type Target = dyn LlmProvider + 'a;
    fn deref(&self) -> &Self::Target {
        match self {
            Self::Owned(provider) => provider.as_ref(),
            Self::Borrowed(provider) => *provider,
        }
    }
}

struct ScopedProvider<'a> {
    inner: ProviderRef<'a>,
    tracker: UsageTracker,
}

impl ScopedProvider<'_> {
    fn prepare(
        &self,
        mut request: ChatRequest,
        tools: bool,
    ) -> Result<(ChatRequest, Option<UsageAttempt>)> {
        self.inner.validate_request(&request, tools)?;
        let tracker = request
            .usage_tracker
            .take()
            .unwrap_or_else(|| self.tracker.clone());
        let attempt = if self.inner.records_usage() {
            request.usage_tracker = Some(tracker);
            None
        } else {
            // Custom providers opt into receipt ownership. Otherwise each
            // invocation remains explicitly unavailable, never known zero.
            Some(tracker.start().map_err(accounting_error)?)
        };
        Ok((request, attempt))
    }
}

fn accounting_error(error: crate::usage::UsageOverflow) -> IronCrewError {
    IronCrewError::Provider(error.to_string())
}

fn settle(result: Result<ChatResponse>, attempt: Option<UsageAttempt>) -> Result<ChatResponse> {
    if let Some(attempt) = attempt {
        attempt
            .finish(UsageReceipt::default())
            .map_err(accounting_error)?;
    }
    result
}

#[async_trait]
impl LlmProvider for ScopedProvider<'_> {
    fn usage_tracker(&self) -> Option<UsageTracker> {
        Some(self.tracker.clone())
    }
    fn records_usage(&self) -> bool {
        true
    }
    fn validate_request(&self, request: &ChatRequest, has_tools: bool) -> Result<()> {
        self.inner.validate_request(request, has_tools)
    }
    fn metrics_family(&self) -> crate::metrics::ProviderFamily {
        self.inner.metrics_family()
    }
    fn execution_fingerprint(&self) -> Result<String> {
        self.inner.execution_fingerprint()
    }
    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse> {
        let (request, attempt) = self.prepare(request, false)?;
        settle(self.inner.chat(request).await, attempt)
    }
    async fn chat_with_tools(
        &self,
        request: ChatRequest,
        tools: &[ToolSchema],
    ) -> Result<ChatResponse> {
        let (request, attempt) = self.prepare(request, !tools.is_empty())?;
        settle(self.inner.chat_with_tools(request, tools).await, attempt)
    }
    async fn chat_stream(
        &self,
        request: ChatRequest,
        tx: tokio::sync::mpsc::Sender<StreamChunk>,
    ) -> Result<ChatResponse> {
        let (request, attempt) = self.prepare(request, false)?;
        settle(self.inner.chat_stream(request, tx).await, attempt)
    }
}
