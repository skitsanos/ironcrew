use super::*;

impl LuaConversationInner {
    /// Persist the current state to the configured store. Safe to call even
    /// for non-persistent sessions — it simply no-ops.
    pub async fn persist(&self) -> Result<(), IronCrewError> {
        let _execution_guard = self.turn_execution_lock.clone().lock_owned().await;
        self.persist_current_snapshot().await
    }

    pub(super) async fn persist_current_snapshot(&self) -> Result<(), IronCrewError> {
        let Some(ref store) = self.store else {
            return Ok(());
        };
        if !self.persistent {
            return Ok(());
        }
        let mut revision = self.revision.lock().await;
        let messages = self.messages.lock().await.clone();
        validate_chat_history(
            &messages,
            self.max_history
                .unwrap_or(DEFAULT_CHAT_HISTORY_MAX_MESSAGES),
            self.history_max_bytes,
            true,
        )?;
        let record = ConversationRecord {
            usage: self.usage_snapshot()?,
            id: self.id.clone(),
            flow_name: self.flow_name.clone(),
            flow_path: self.flow_path.clone(),
            agent_name: self.agent.name.clone(),
            execution: self.execution.clone(),
            messages,
            created_at: self.created_at.clone(),
            updated_at: chrono::Utc::now().to_rfc3339(),
            revision: *revision,
        };
        *revision = store.save_conversation(&record).await?;
        Ok(())
    }
}
