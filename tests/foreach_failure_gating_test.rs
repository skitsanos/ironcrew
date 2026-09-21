//! IC-030: a foreach task whose every item fails must gate its dependents the
//! same way a failed standard task does. Before the fix it emitted
//! `TaskCompleted`, stayed out of `failed_tasks`, and let dependents run with
//! no usable input.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use ironcrew::engine::agent::Agent;
use ironcrew::engine::crew::{Crew, ProviderConfig, run_crew};
use ironcrew::engine::memory::MemoryStore;
use ironcrew::engine::task::Task;
use ironcrew::llm::provider::{ChatRequest, ChatResponse, LlmProvider, ToolSchema};
use ironcrew::tools::registry::ToolRegistry;
use ironcrew::utils::error::{IronCrewError, Result};

/// Fails every foreach item, and records whether the dependent task ever ran.
struct AlwaysFailingProvider {
    dependent_calls: AtomicUsize,
}

#[async_trait]
impl LlmProvider for AlwaysFailingProvider {
    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse> {
        let prompt = request
            .messages
            .iter()
            .filter_map(|message| message.content.clone())
            .collect::<Vec<_>>()
            .join("\n");
        if prompt.contains("DEPENDENT-TASK") {
            self.dependent_calls.fetch_add(1, Ordering::SeqCst);
            return Ok(ChatResponse {
                content: Some("dependent ran".into()),
                ..Default::default()
            });
        }
        Err(IronCrewError::Validation("item failed".into()))
    }

    async fn chat_with_tools(
        &self,
        request: ChatRequest,
        _tools: &[ToolSchema],
    ) -> Result<ChatResponse> {
        self.chat(request).await
    }
}

fn crew_with_failing_foreach() -> Crew {
    let mut crew = Crew::new(
        "gate dependents when every foreach item fails".into(),
        ProviderConfig {
            provider: "openai".into(),
            model: "test-model".into(),
            base_url: None,
            api_key: Some("test-key".into()),
        },
        MemoryStore::ephemeral(),
    );

    crew.add_agent(Agent {
        name: "worker".into(),
        goal: "process items".into(),
        ..Default::default()
    })
    .expect("agent is valid");

    crew.add_task(Task {
        name: "source".into(),
        description: "produce the item list".into(),
        agent: Some("worker".into()),
        ..Default::default()
    })
    .expect("source task is valid");

    crew.add_task(Task {
        name: "fanout".into(),
        description: "handle ${item}".into(),
        agent: Some("worker".into()),
        foreach_source: Some("items".into()),
        foreach_as: Some("item".into()),
        ..Default::default()
    })
    .expect("foreach task is valid");

    crew.add_task(Task {
        name: "dependent".into(),
        description: "DEPENDENT-TASK summarize ${results.fanout.output}".into(),
        agent: Some("worker".into()),
        depends_on: vec!["fanout".into()],
        ..Default::default()
    })
    .expect("dependent task is valid");

    crew
}

#[tokio::test]
async fn wholly_failed_foreach_skips_its_dependents() {
    let crew = crew_with_failing_foreach();
    crew.memory
        .set(
            "items".into(),
            serde_json::json!(["alpha", "beta", "gamma"]),
        )
        .await
        .expect("seed foreach source");

    let provider = Arc::new(AlwaysFailingProvider {
        dependent_calls: AtomicUsize::new(0),
    });
    let registry = ToolRegistry::new();

    let results = run_crew(&crew, provider.clone(), &registry)
        .await
        .expect("run completes without aborting");

    let dependent = results
        .iter()
        .find(|result| result.task == "dependent")
        .expect("dependent task is present in the results");

    assert_eq!(
        provider.dependent_calls.load(Ordering::SeqCst),
        0,
        "dependent task executed even though every foreach item failed"
    );
    assert!(
        dependent.output.contains("Skipped"),
        "dependent should be recorded as skipped, got: {}",
        dependent.output
    );

    let fanout = results
        .iter()
        .find(|result| result.task == "fanout")
        .expect("foreach task is present in the results");
    assert!(
        !fanout.success,
        "a foreach with no successful item must fail"
    );
    assert!(
        !fanout.output.contains("Skipped:"),
        "the foreach must have run its items, not taken a skip path: {}",
        fanout.output
    );
    assert!(
        fanout.output.contains("Error:"),
        "expected per-item errors in the foreach output: {}",
        fanout.output
    );
}

#[tokio::test]
async fn partially_failed_foreach_still_runs_dependents() {
    // One item succeeds, so the foreach output carries usable content and the
    // dependent must still run — the fix must not over-gate.
    struct PartialProvider {
        dependent_calls: AtomicUsize,
    }

    #[async_trait]
    impl LlmProvider for PartialProvider {
        async fn chat(&self, request: ChatRequest) -> Result<ChatResponse> {
            let prompt = request
                .messages
                .iter()
                .filter_map(|message| message.content.clone())
                .collect::<Vec<_>>()
                .join("\n");
            if prompt.contains("DEPENDENT-TASK") {
                self.dependent_calls.fetch_add(1, Ordering::SeqCst);
                return Ok(ChatResponse {
                    content: Some("dependent ran".into()),
                    ..Default::default()
                });
            }
            if prompt.contains("alpha") {
                return Ok(ChatResponse {
                    content: Some("alpha handled".into()),
                    ..Default::default()
                });
            }
            Err(IronCrewError::Validation("item failed".into()))
        }

        async fn chat_with_tools(
            &self,
            request: ChatRequest,
            _tools: &[ToolSchema],
        ) -> Result<ChatResponse> {
            self.chat(request).await
        }
    }

    let crew = crew_with_failing_foreach();
    crew.memory
        .set("items".into(), serde_json::json!(["alpha", "beta"]))
        .await
        .expect("seed foreach source");

    let provider = Arc::new(PartialProvider {
        dependent_calls: AtomicUsize::new(0),
    });
    let registry = ToolRegistry::new();

    run_crew(&crew, provider.clone(), &registry)
        .await
        .expect("run completes");

    assert_eq!(
        provider.dependent_calls.load(Ordering::SeqCst),
        1,
        "dependent must still run when the foreach produced some output"
    );
}
