//! End-to-end integration tests for the stuck-run reconciler.
//!
//! T4 — full happy-path lifecycle via Crew::run against a stub
//!      provider. After the run, one RunRecord exists in Success
//!      state. Proves the two-phase path doesn't regress.
//!
//! T5 — simulated crash. Call save_run_intent directly; skip
//!      completion; invoke reconcile_stuck_runs. Record is now
//!      Abandoned with finished_at set.

use std::sync::Arc;

use ironcrew::engine::reconciler::reconcile_stuck_runs;
use ironcrew::engine::run_history::{JsonFileStore, RunStatus};
use ironcrew::engine::store::StateStore;

// ─── T5 ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_05_simulated_crash_reconciles_cleanly() {
    let dir = tempfile::tempdir().unwrap();
    let store: Arc<dyn StateStore> =
        Arc::new(JsonFileStore::new(dir.path().to_path_buf()).unwrap());

    // Simulate a crashed run: write intent but NOT completion.
    let run_id = store
        .save_run_intent(
            Some("crashed-run".into()),
            "demo-flow",
            "2026-04-23T10:00:00Z",
            2,
            3,
            &[],
        )
        .await
        .unwrap();
    assert_eq!(run_id, "crashed-run");

    let before = store.get_run("crashed-run").await.unwrap();
    assert_eq!(before.status, RunStatus::Running);
    assert_eq!(before.finished_at, "");

    // Run the reconciler (as if a new process is booting).
    let reconciled = reconcile_stuck_runs(&store).await.unwrap();
    assert_eq!(reconciled, 1);

    let after = store.get_run("crashed-run").await.unwrap();
    assert_eq!(after.status, RunStatus::Abandoned);
    assert!(!after.finished_at.is_empty());
    assert_eq!(after.task_results.len(), 0);
}

// ─── T4 ────────────────────────────────────────────────────────────────────

struct StubProvider;

#[async_trait::async_trait]
impl ironcrew::llm::provider::LlmProvider for StubProvider {
    async fn chat(
        &self,
        _request: ironcrew::llm::provider::ChatRequest,
    ) -> ironcrew::utils::error::Result<ironcrew::llm::provider::ChatResponse> {
        Ok(ironcrew::llm::provider::ChatResponse {
            content: Some("reconciler-test-output".into()),
            reasoning: None,
            tool_calls: vec![],
            usage: None,
        })
    }

    async fn chat_with_tools(
        &self,
        request: ironcrew::llm::provider::ChatRequest,
        _tools: &[ironcrew::llm::provider::ToolSchema],
    ) -> ironcrew::utils::error::Result<ironcrew::llm::provider::ChatResponse> {
        self.chat(request).await
    }
}

#[tokio::test]
async fn test_04_full_lifecycle_writes_success_record() {
    use ironcrew::engine::agent::Agent;
    use ironcrew::engine::crew::{Crew, ProviderConfig};
    use ironcrew::engine::memory::MemoryStore;
    use ironcrew::engine::task::Task;
    use ironcrew::llm::provider::LlmProvider;
    use ironcrew::tools::registry::ToolRegistry;

    let dir = tempfile::tempdir().unwrap();
    let store: Arc<dyn StateStore> =
        Arc::new(JsonFileStore::new(dir.path().to_path_buf()).unwrap());

    let provider_config = ProviderConfig {
        provider: "stub".into(),
        model: "stub-model".into(),
        base_url: None,
        api_key: None,
    };
    let memory = MemoryStore::ephemeral();

    let mut crew = Crew::new("integration-test-crew".into(), provider_config, memory);
    crew.agents.push(Agent {
        name: "assistant".into(),
        goal: "answer".into(),
        ..Default::default()
    });
    crew.tasks.push(Task {
        name: "task-1".into(),
        agent: Some("assistant".into()),
        description: "say hi".into(),
        ..Default::default()
    });

    let provider: Arc<dyn LlmProvider> = Arc::new(StubProvider);
    let registry = ToolRegistry::new();

    // Write intent directly (mimics what crew:run() does in the two-phase path).
    let run_id = store
        .save_run_intent(
            None,
            &crew.goal,
            "2026-04-23T10:00:00Z",
            crew.agents.len(),
            crew.tasks.len(),
            &[],
        )
        .await
        .unwrap();

    // Run the crew.
    let run_start = chrono::Utc::now();
    let results = crew.run(provider, &registry).await.unwrap();
    let run_end = chrono::Utc::now();
    let total_ms = (run_end - run_start).num_milliseconds().max(0) as u64;

    // Derive terminal fields via the existing helper, then complete.
    let record = crew.create_run_record(
        Some(run_id.clone()),
        &results,
        &run_start.to_rfc3339(),
        &run_end.to_rfc3339(),
        total_ms,
    );
    store
        .update_run_completion(
            &run_id,
            record.status.clone(),
            &run_end.to_rfc3339(),
            total_ms,
            record.task_results.clone(),
            record.total_tokens,
            record.cached_tokens,
        )
        .await
        .unwrap();

    let saved = store.get_run(&run_id).await.unwrap();
    assert_eq!(saved.status, RunStatus::Success);
    assert_eq!(saved.task_results.len(), 1);
    assert_eq!(saved.task_results[0].task, "task-1");
    assert_eq!(saved.task_results[0].output, "reconciler-test-output");
}
