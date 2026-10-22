//! IC-052: recovery diagnostics are data, never runtime templates.
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use ironcrew::engine::agent::Agent;
use ironcrew::engine::memory::MemoryStore;
use ironcrew::engine::task::{Task, TaskResult};
use ironcrew::engine::task_runner::handle_task_error;
use ironcrew::llm::provider::{ChatRequest, ChatResponse, LlmProvider, ToolSchema};
use ironcrew::llm::scope::with_usage_tracker;
use ironcrew::tools::registry::ToolRegistry;
use ironcrew::usage::{UsageCounts, UsageCoverage, UsageReceipt, UsageTracker};
use ironcrew::utils::error::{IronCrewError, Result};

#[derive(Default)]
struct Recorder {
    requests: Mutex<Vec<(String, String)>>,
    mode: u8,
    entered: tokio::sync::Notify,
}

#[async_trait]
impl LlmProvider for Recorder {
    fn records_usage(&self) -> bool {
        true
    }

    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse> {
        let prompt = request
            .messages
            .iter()
            .filter_map(|message| message.content.as_deref())
            .collect::<Vec<_>>()
            .join("\n");
        self.requests.lock().unwrap().push((request.model, prompt));
        let tracker = request
            .usage_tracker
            .expect("handler must supply a usage scope");
        let mut attempt = tracker.start().unwrap();
        let receipt = UsageReceipt::from_counts(
            UsageCounts {
                prompt_tokens: Some(10),
                completion_tokens: Some(3),
                total_tokens: Some(13),
                cached_tokens: Some(0),
                cache_write_tokens: Some(0),
                reasoning_tokens: Some(0),
            },
            self.mode != 2,
        );
        attempt.observe(receipt.clone());
        if self.mode == 2 {
            self.entered.notify_one();
            std::future::pending::<()>().await;
        }
        attempt.finish(receipt.clone()).unwrap();
        if self.mode == 1 {
            return Err(IronCrewError::Provider("Authored handler failure".into()));
        }
        Ok(ChatResponse {
            content: Some("recovered".into()),
            usage: receipt,
            ..Default::default()
        })
    }

    async fn chat_with_tools(
        &self,
        request: ChatRequest,
        _: &[ToolSchema],
    ) -> Result<ChatResponse> {
        self.chat(request).await
    }
}

async fn recover(provider: Arc<dyn LlmProvider>, message: &str, source: &str) -> TaskResult {
    let primary = Task {
        name: "primary".into(),
        on_error: Some("fallback".into()),
        ..Default::default()
    };
    let fallback = Task {
        name: "fallback".into(),
        agent: Some("worker".into()),
        model: Some("task-model".into()),
        description: "Recover from the original evidence".into(),
        expected_output: Some("Expected ${results.source.output}".into()),
        context: Some("Configured ${results.source.output}".into()),
        depends_on: vec!["source".into()],
        ..Default::default()
    };
    let agents = [Agent {
        name: "worker".into(),
        model: Some("agent-model".into()),
        ..Default::default()
    }];
    let results = HashMap::from([(
        "source".into(),
        TaskResult {
            task: "source".into(),
            agent: "worker".into(),
            output: source.into(),
            success: true,
            duration_ms: 1,
            usage: Default::default(),
            reasoning: None,
        },
    )]);
    handle_task_error(
        &primary,
        "worker",
        message,
        &[primary.clone(), fallback],
        &agents,
        provider,
        &ToolRegistry::new(),
        &results,
        &MemoryStore::ephemeral(),
        "crew-model",
        1,
    )
    .await
    .expect("configured recovery must run")
}

#[tokio::test]
async fn diagnostics_stay_literal_while_configured_templates_resolve() {
    let recorder = Arc::new(Recorder::default());
    let tracker = UsageTracker::default();
    let provider = with_usage_tracker(recorder.clone(), tracker.clone());
    let message = "Authored ${env.IC052_FAKE_SECRET} ${results.source.output} ${results.missing.output} \"quotes\" {braces}\0\n\t🧪";
    let result = recover(provider, message, "PRIOR_EVIDENCE").await;
    assert!(result.success);
    assert_eq!(result.task, "fallback");
    assert_eq!(result.output, "recovered");
    let requests = recorder.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].0, "agent-model");
    let prompt = &requests[0].1;
    assert!(
        prompt.contains(message),
        "error diagnostics were interpreted or changed"
    );
    assert!(prompt.contains("Expected output: Expected PRIOR_EVIDENCE"));
    assert!(prompt.contains("Additional context: Configured PRIOR_EVIDENCE"));
    assert!(prompt.contains("Result from 'source': PRIOR_EVIDENCE"));
    let snapshot = tracker.snapshot().unwrap();
    assert_eq!(snapshot, result.usage);
    assert_eq!(snapshot.settled.requests(), 1);
    assert_eq!(snapshot.settled.total_tokens().known(), Some(13));
    assert_eq!(snapshot.in_flight, 0);
}

#[tokio::test]
async fn large_diagnostic_cannot_displace_dependency_evidence() {
    let recorder = Arc::new(Recorder::default());
    let source = format!("PRIOR_BEGIN{}PRIOR_END", "x".repeat(4096));
    let message = "🧪".repeat(200_000);
    let result = recover(recorder.clone(), &message, &source).await;
    assert!(result.success);
    let requests = recorder.requests.lock().unwrap();
    let prompt = &requests[0].1;
    assert!(prompt.contains(&format!("Result from 'source': {source}")));
    let user_prompt = prompt.split_once("\nTask: ").unwrap().1;
    assert!(user_prompt.chars().count() + "Task: ".len() <= 102_400);
    assert!(user_prompt.contains("[... prompt truncated due to size limit]"));
}

#[tokio::test]
async fn failed_handler_keeps_one_checked_receipt() {
    let recorder = Arc::new(Recorder {
        mode: 1,
        ..Default::default()
    });
    let tracker = UsageTracker::default();
    let result = recover(
        with_usage_tracker(recorder.clone(), tracker.clone()),
        "Authored error",
        "prior",
    )
    .await;
    assert!(!result.success);
    assert_eq!(recorder.requests.lock().unwrap().len(), 1);
    let usage = tracker.snapshot().unwrap();
    assert_eq!(usage, result.usage);
    assert_eq!(usage.settled.requests(), 1);
    assert_eq!(usage.settled.total_tokens().known(), Some(13));
    assert_eq!(usage.coverage, UsageCoverage::Complete);
    assert_eq!(usage.in_flight, 0);
}

#[tokio::test]
async fn cancelled_handler_preserves_partial_usage_without_leaving_in_flight_work() {
    let recorder = Arc::new(Recorder {
        mode: 2,
        ..Default::default()
    });
    let tracker = UsageTracker::default();
    let provider = with_usage_tracker(recorder.clone(), tracker.clone());
    let mut pending = Box::pin(recover(provider, "Authored error", "prior"));
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        tokio::select! {
            _ = recorder.entered.notified() => {},
            _ = &mut pending => panic!("held provider unexpectedly completed"),
        }
    })
    .await
    .unwrap();
    drop(pending);
    assert_eq!(recorder.requests.lock().unwrap().len(), 1);
    let usage = tracker.snapshot().unwrap();
    assert_eq!(usage.settled.requests(), 1);
    assert_eq!(usage.settled.total_tokens().known(), Some(13));
    assert_eq!(usage.coverage, UsageCoverage::Partial);
    assert_eq!(usage.in_flight, 0);
}
