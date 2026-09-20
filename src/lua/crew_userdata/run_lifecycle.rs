use crate::engine::run_history::{RunCompletion, RunTransition};
use std::sync::Arc;
use tokio::sync::Mutex;

// ---------------------------------------------------------------------------
// API-owned run lifecycle
// ---------------------------------------------------------------------------

/// Completion produced by `crew:run()` while the enclosing HTTP-owned Lua
/// entrypoint is still executing.
///
/// CLI runs do not install this context and continue to persist completion
/// directly from `crew:run()`. The HTTP runner installs it so flow-level Lua
/// can safely continue after the crew finishes (including suspending on a
/// later `crew:ask_human()`) without making the durable run terminal early.
#[derive(Debug, Clone)]
pub(crate) struct StagedRunCompletion {
    pub(crate) run_id: String,
    pub(crate) completion: RunCompletion,
}

#[derive(Debug, Clone)]
pub(crate) struct StagedRunSummary {
    pub(crate) run_id: String,
    pub(crate) status: crate::engine::run_history::RunStatus,
    pub(crate) duration_ms: u64,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct ApiRunLifecycle {
    completion: Arc<Mutex<Option<StagedRunCompletion>>>,
}

impl ApiRunLifecycle {
    pub(super) async fn stage(&self, run_id: String, completion: RunCompletion) {
        *self.completion.lock().await = Some(StagedRunCompletion { run_id, completion });
    }

    pub(crate) async fn take_completion(&self) -> Option<StagedRunCompletion> {
        self.completion.lock().await.take()
    }

    pub(crate) async fn completion_summary(&self) -> Option<StagedRunSummary> {
        self.completion
            .lock()
            .await
            .as_ref()
            .map(|staged| StagedRunSummary {
                run_id: staged.run_id.clone(),
                status: staged.completion.status.clone(),
                duration_ms: staged.completion.duration_ms,
            })
    }
}

/// Stage HTTP completion; persist CLI completion at the crew boundary.
pub(super) async fn complete_run(
    store: &Arc<dyn crate::engine::store::StateStore>,
    lifecycle: Option<&ApiRunLifecycle>,
    run_id: &str,
    completion: RunCompletion,
) -> crate::utils::error::Result<()> {
    if let Some(lifecycle) = lifecycle {
        lifecycle.stage(run_id.to_owned(), completion).await;
        return Ok(());
    }
    let status = completion.status.clone();
    let duration_ms = completion.duration_ms;
    let transition = store.update_run_completion(run_id, completion).await;
    let outcome = match &transition {
        Ok(RunTransition::Applied) => {
            if let Some(outcome) = crate::metrics::RunOutcome::from_status(&status) {
                crate::metrics::record_run(
                    outcome,
                    Some(std::time::Duration::from_millis(duration_ms)),
                );
            }
            crate::metrics::TerminalOutcome::Success
        }
        Ok(RunTransition::AlreadyTerminal(_)) => crate::metrics::TerminalOutcome::Fenced,
        Err(_) => {
            crate::metrics::record_store_failure(
                crate::metrics::StoreOperation::TerminalPersistence,
            );
            crate::metrics::TerminalOutcome::Error
        }
    };
    crate::metrics::record_terminal_persistence(crate::metrics::TerminalScope::RunRecord, outcome);
    transition.map(|_| ())
}
