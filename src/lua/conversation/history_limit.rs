use crate::llm::provider::{DEFAULT_CHAT_HISTORY_MAX_MESSAGES, HARD_CHAT_HISTORY_MAX_MESSAGES};

/// Resolve the default max_history cap when no explicit Lua-side value is
/// provided. Honors a positive `IRONCREW_CONVERSATION_MAX_HISTORY` up to the
/// process hard ceiling, falling back to a safe 50-message cap. Shared with non-conversation
/// consumers (e.g. `AgentAsTool` finalization) so they apply the same
/// policy as the user-facing `crew:conversation()` path.
pub(crate) fn default_max_history() -> Option<usize> {
    let env_default = std::env::var("IRONCREW_CONVERSATION_MAX_HISTORY")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .map(|value| value.min(HARD_CHAT_HISTORY_MAX_MESSAGES))
        .unwrap_or(DEFAULT_CHAT_HISTORY_MAX_MESSAGES);
    Some(env_default)
}
