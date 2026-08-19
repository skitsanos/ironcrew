//! IC-023: a task whose output is within the task-result cap but larger than
//! the (smaller) memory value cap must not fail the run after the provider
//! work is already done. End-of-phase memory persistence is best-effort.

use std::sync::Arc;

use async_trait::async_trait;
use ironcrew::engine::agent::Agent;
use ironcrew::engine::crew::{Crew, ProviderConfig, run_crew};
use ironcrew::engine::memory::MemoryStore;
use ironcrew::engine::task::Task;
use ironcrew::llm::provider::{ChatRequest, ChatResponse, LlmProvider, ToolSchema};
use ironcrew::tools::registry::ToolRegistry;
use ironcrew::utils::error::Result;

/// Emits output above the default 1 MiB memory value cap but well below the
/// default 8 MiB task-result cap.
const OUTPUT_BYTES: usize = 2 * 1024 * 1024;

struct LargeOutputProvider;

#[async_trait]
impl LlmProvider for LargeOutputProvider {
    async fn chat(&self, _request: ChatRequest) -> Result<ChatResponse> {
        Ok(ChatResponse {
            content: Some("x".repeat(OUTPUT_BYTES)),
            ..Default::default()
        })
    }

    async fn chat_with_tools(
        &self,
        request: ChatRequest,
        _tools: &[ToolSchema],
    ) -> Result<ChatResponse> {
        self.chat(request).await
    }
}

#[tokio::test]
async fn oversized_task_output_does_not_fail_the_run() {
    let mut crew = Crew::new(
        "survive a memory value cap smaller than the task result cap".into(),
        ProviderConfig {
            provider: "openai".into(),
            model: "test-model".into(),
            base_url: None,
            api_key: Some("test-key".into()),
        },
        MemoryStore::ephemeral(),
    );

    crew.add_agent(Agent {
        name: "writer".into(),
        goal: "produce a large report".into(),
        ..Default::default()
    })
    .expect("agent is valid");

    crew.add_task(Task {
        name: "report".into(),
        description: "write the report".into(),
        agent: Some("writer".into()),
        ..Default::default()
    })
    .expect("task is valid");

    let results = run_crew(&crew, Arc::new(LargeOutputProvider), &ToolRegistry::new())
        .await
        .expect("run must succeed even though the result exceeds the memory value cap");

    let report = results
        .iter()
        .find(|result| result.task == "report")
        .expect("report task is present");
    assert!(report.success, "task itself must be reported as successful");
    assert_eq!(
        report.output.len(),
        OUTPUT_BYTES,
        "the completed output must be returned to the caller intact"
    );
}
