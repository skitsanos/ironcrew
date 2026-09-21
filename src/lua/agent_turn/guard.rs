use crate::llm::provider::ChatMessage;
use std::ops::{Deref, DerefMut};

/// Rolls back the currently-active user turn if the future is cancelled or
/// returns before `commit`. Keeping this guard alive across provider/tool
/// awaits makes Tokio timeout and shutdown cancellation history-safe.
pub(crate) struct ActiveTurnGuard<'a> {
    history: &'a mut Vec<ChatMessage>,
    /// Exact transcript that existed before the active user message. History
    /// limiting can drain old turn groups while a provider/tool request is in
    /// flight, so remembering only the active user's index is not sufficient:
    /// cancellation would otherwise keep the drain and silently lose older
    /// persisted context.
    rollback_snapshot: Option<Vec<ChatMessage>>,
    committed: bool,
}

impl<'a> ActiveTurnGuard<'a> {
    pub(crate) fn new(history: &'a mut Vec<ChatMessage>) -> Self {
        let rollback_snapshot = history
            .iter()
            .rposition(|message| message.role == "user")
            .map(|active_start| history[..active_start].to_vec());
        Self {
            history,
            rollback_snapshot,
            committed: false,
        }
    }

    pub(crate) fn commit(&mut self) {
        self.committed = true;
        self.rollback_snapshot = None;
    }
}

impl Deref for ActiveTurnGuard<'_> {
    type Target = Vec<ChatMessage>;

    fn deref(&self) -> &Self::Target {
        self.history
    }
}

impl DerefMut for ActiveTurnGuard<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.history
    }
}

impl Drop for ActiveTurnGuard<'_> {
    fn drop(&mut self) {
        if !self.committed
            && let Some(snapshot) = self.rollback_snapshot.take()
        {
            *self.history = snapshot;
        }
    }
}
