use super::*;

/// Persist one terminal transition for an HTTP-owned run. A normal
/// `crew:run()` contributes its staged, result-bearing completion only after
/// the enclosing Lua entrypoint ends. If a task failed before Lua could create
/// the intent, create a minimal fallback record only after confirming the run
/// is genuinely absent.
pub(super) struct TerminalPersistence<'a> {
    pub(super) run_id: &'a str,
    pub(super) flow: &'a str,
    pub(super) started_at: &'a str,
    pub(super) tags: &'a [String],
    pub(super) status: RunStatus,
    pub(super) duration_ms: u64,
    pub(super) usage: crate::usage::UsageSnapshot,
    /// Full crew completion retained by the API lifecycle until the enclosing
    /// Lua entrypoint has returned. Absent for pre-crew failures, aborts,
    /// timeouts, and flows that never call `crew:run()`.
    pub(super) completion: Option<&'a RunCompletion>,
}

pub(super) async fn persist_terminal_outcome(
    store: &Arc<dyn crate::engine::store::StateStore>,
    terminal: TerminalPersistence<'_>,
) -> Result<(RunStatus, bool), IronCrewError> {
    let synthesized;
    let completion = match terminal.completion {
        Some(completion) => completion,
        None => {
            synthesized = RunCompletion {
                status: terminal.status.clone(),
                finished_at: chrono::Utc::now().to_rfc3339(),
                duration_ms: terminal.duration_ms,
                task_results: Vec::new(),
                usage: terminal.usage,
            };
            &synthesized
        }
    };
    let completion_status = completion.status.clone();

    match store
        .update_run_completion(terminal.run_id, completion.clone())
        .await
    {
        Ok(RunTransition::Applied) => return Ok((completion_status.clone(), true)),
        Ok(RunTransition::AlreadyTerminal(status)) => return Ok((status, false)),
        Err(update_error) => match store.get_run(terminal.run_id).await {
            Ok(record) if record.status.is_terminal() => return Ok((record.status, false)),
            Ok(record) => {
                return Err(IronCrewError::Validation(format!(
                    "Failed to persist terminal outcome for run '{}': {}; durable status remains '{}'",
                    terminal.run_id, update_error, record.status
                )));
            }
            Err(get_error) if run_not_found(&get_error, terminal.run_id) => {}
            Err(get_error) => {
                return Err(IronCrewError::Validation(format!(
                    "Could not verify run '{}' after terminal update failed (update: {}; read: {})",
                    terminal.run_id, update_error, get_error
                )));
            }
        },
    }

    let fallback = RunIntent {
        suggested_id: Some(terminal.run_id.to_string()),
        flow_name: terminal.flow.to_string(),
        flow: terminal.flow.to_string(),
        started_at: terminal.started_at.to_string(),
        agent_count: 0,
        task_count: 0,
        tags: terminal.tags.to_vec(),
    };
    if let Err(error) = store.save_run_intent(fallback).await {
        if matches!(error, IronCrewError::OwnerDraining { .. }) {
            return Err(error);
        }
        return Err(IronCrewError::Validation(format!(
            "Failed to create fallback intent for terminal run '{}': {}",
            terminal.run_id, error
        )));
    }

    match store
        .update_run_completion(terminal.run_id, completion.clone())
        .await
    {
        Ok(RunTransition::Applied) => Ok((completion_status, true)),
        Ok(RunTransition::AlreadyTerminal(status)) => Ok((status, false)),
        Err(error) => Err(IronCrewError::Validation(format!(
            "Failed to persist terminal outcome for fallback run '{}': {}",
            terminal.run_id, error
        ))),
    }
}
