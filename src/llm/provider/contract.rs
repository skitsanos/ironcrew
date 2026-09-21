use super::*;
use async_trait::async_trait;

#[async_trait]
pub trait LlmProvider: Send + Sync {
    /// Implementations opting in must reserve exact input plus bounded output
    /// before every dispatch and reconcile even on failure/cancellation.
    fn supports_token_budget(&self) -> bool {
        false
    }
    /// Explicit execution scope. Shared Runtime providers must remain unbound.
    fn usage_tracker(&self) -> Option<crate::usage::UsageTracker> {
        None
    }

    /// Opt-in receipt ownership: settle request.usage_tracker once per dispatch,
    /// retaining receipts on errors/cancellation without re-adding nested work.
    /// Otherwise execution wrappers count each invocation as unavailable.
    fn records_usage(&self) -> bool {
        false
    }

    /// Dispatch-level metrics are owned by the implementation, including
    /// failure/cancellation. Forwarding wrappers must preserve this capability.
    fn records_usage_metrics(&self) -> bool {
        false
    }

    /// Offline request-shape check, also used while constructing Lua crews.
    /// Custom Rust providers own their contract and may override this hook.
    fn validate_request(&self, _request: &ChatRequest, _has_tools: bool) -> Result<()> {
        Ok(())
    }

    /// Fixed, non-secret provider implementation family used for process
    /// metrics. Custom providers default to the bounded `other` family.
    fn metrics_family(&self) -> crate::metrics::ProviderFamily {
        crate::metrics::ProviderFamily::Other
    }

    /// Canonical identity of the effective, non-secret provider behavior used
    /// to resume durable conversations. Providers that do not implement this
    /// must fail closed for persistent conversation construction.
    fn execution_fingerprint(&self) -> Result<String> {
        Err(IronCrewError::Validation(
            "Provider does not expose a durable conversation execution fingerprint".into(),
        ))
    }

    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse>;
    async fn chat_with_tools(
        &self,
        request: ChatRequest,
        tools: &[ToolSchema],
    ) -> Result<ChatResponse>;

    /// Stream a chat response. Default implementation falls back to non-streaming.
    async fn chat_stream(
        &self,
        request: ChatRequest,
        tx: tokio::sync::mpsc::Sender<StreamChunk>,
    ) -> Result<ChatResponse> {
        let response = self.chat(request).await?;
        if let Some(ref content) = response.content {
            let _ = tx.send(StreamChunk::Text(content.clone())).await;
        }
        let _ = tx.send(StreamChunk::Done).await;
        Ok(response)
    }
}
