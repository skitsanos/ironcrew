use super::*;

impl AgentDialog {
    /// Inclusive session usage at the latest in-process checkpoint.
    pub fn usage_snapshot(&self) -> Result<crate::usage::UsageSnapshot, IronCrewError> {
        crate::llm::scope::snapshot(&self.usage)
    }
    /// Persist the current dialog state to the configured store.
    /// No-ops for non-persistent sessions.
    pub async fn persist(&self) -> Result<(), IronCrewError> {
        let Some(ref store) = self.store else {
            return Ok(());
        };
        if !self.persistent {
            return Ok(());
        }
        let mut revision = self.revision.lock().await;
        let next_index = *self.next_index.lock().await;
        let transcript_guard = self.transcript.lock().await;
        validate_transcript(
            &transcript_guard,
            &self.agents,
            self.max_history.unwrap_or(DEFAULT_DIALOG_MAX_HISTORY),
            self.max_turns,
            self.history_max_bytes,
            next_index,
        )?;
        let transcript: Vec<DialogTurn> = transcript_guard.iter().cloned().collect();
        drop(transcript_guard);
        let stopped = *self.stopped.lock().await;
        let stop_reason = self.stop_reason.lock().await.clone();
        let record = DialogStateRecord {
            usage: self.usage_snapshot()?,
            id: self.id.clone(),
            flow_name: self.flow_name.clone(),
            flow_path: self.flow_path.clone(),
            agent_names: self.agents.iter().map(|a| a.name.clone()).collect(),
            starter: self.starter.clone(),
            transcript,
            next_index,
            stopped,
            stop_reason,
            created_at: self.created_at.clone(),
            updated_at: chrono::Utc::now().to_rfc3339(),
            revision: *revision,
        };
        *revision = store.save_dialog_state(&record).await?;
        Ok(())
    }
}
