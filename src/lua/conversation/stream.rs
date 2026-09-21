use super::*;

impl LuaConversationInner {
    /// Streaming no-tools turn. The user message has already been pushed
    /// by the caller; this method issues one streaming provider call,
    /// appends the assistant reply, and returns (content, reasoning).
    ///
    /// Tool-call rounds are not supported here by design — the shared
    /// helper owns that path and does not stream. Callers must check
    /// `has_tools` before dispatching to this method.
    pub(super) async fn run_turn_streaming_no_tools(
        &self,
        history: &mut Vec<ChatMessage>,
        tracker: Option<crate::usage::UsageTracker>,
    ) -> Result<(String, Option<String>), IronCrewError> {
        let mut active_turn = ActiveTurnGuard::new(history);
        let messages_snapshot: Vec<ChatMessage> = active_turn.clone();

        let mut request = self
            .agent
            .chat_request(self.model.clone(), messages_snapshot);
        request.usage_tracker = tracker;

        let response = self.call_streaming(request).await?;

        let content = crate::llm::final_response::require_final_content(response.content)?;

        active_turn.push(ChatMessage::assistant(Some(content.clone()), None));
        enforce_conversation_history_limits(
            &mut active_turn,
            self.max_history
                .unwrap_or(DEFAULT_CHAT_HISTORY_MAX_MESSAGES),
            self.history_max_bytes,
        )?;
        active_turn.commit();

        let reasoning = response.reasoning.map(|reasoning| {
            let limit = self.tool_registry.max_reasoning_bytes();
            let mut bounded = String::new();
            if append_text_bounded(&mut bounded, &reasoning, limit) {
                tracing::warn!(
                    conversation = %self.id,
                    limit,
                    "Reasoning text was truncated to the configured byte limit"
                );
            }
            bounded
        });

        Ok((content, reasoning))
    }

    /// Stream a request to stderr (with dim reasoning) and return the response.
    async fn call_streaming(&self, request: ChatRequest) -> Result<ChatResponse, IronCrewError> {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<StreamChunk>(100);

        let print_handle = tokio::spawn(async move {
            use std::io::Write;
            while let Some(chunk) = rx.recv().await {
                match chunk {
                    StreamChunk::Text(text) => {
                        eprint!("{}", text);
                        std::io::stderr().flush().ok();
                    }
                    StreamChunk::Thinking(text) => {
                        eprint!("\x1b[90m{}\x1b[0m", text);
                        std::io::stderr().flush().ok();
                    }
                    StreamChunk::Done => {
                        eprintln!();
                    }
                    StreamChunk::Error(e) => {
                        eprintln!("\n[Stream error: {}]", e);
                    }
                    _ => {}
                }
            }
        });

        let result = self.provider.chat_stream(request, tx).await;
        print_handle.await.ok();
        result
    }
}
