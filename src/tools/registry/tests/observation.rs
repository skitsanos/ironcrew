use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use tokio::sync::Notify;

use super::super::ToolRegistry;
use crate::llm::provider::ToolSchema;
use crate::tools::{Tool, ToolCallContext};
use crate::utils::error::Result;

fn sample_value(series: &str) -> u64 {
    let mut body = String::new();
    crate::metrics::append_prometheus(&mut body);
    body.lines()
        .find_map(|line| {
            line.strip_prefix(series)
                .and_then(|value| value.strip_prefix(' '))
                .and_then(|value| value.parse().ok())
        })
        .unwrap_or_else(|| panic!("missing metric series: {series}"))
}

struct BlockingTool {
    entered: AtomicUsize,
    release: Notify,
}

#[async_trait]
impl Tool for BlockingTool {
    fn name(&self) -> &str {
        "attacker-controlled-tool-name"
    }

    fn description(&self) -> &str {
        "test"
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: self.name().into(),
            description: self.description().into(),
            parameters: serde_json::json!({"type": "object"}),
        }
    }

    async fn execute(&self, _args: serde_json::Value, _ctx: &ToolCallContext) -> Result<String> {
        self.entered.fetch_add(1, Ordering::AcqRel);
        self.release.notified().await;
        unreachable!("test call is cancelled before release")
    }
}

#[tokio::test]
async fn cancellation_is_recorded_without_exposing_tool_name() {
    let cancelled = "ironcrew_tool_calls_total{outcome=\"cancelled\"}";
    let before = sample_value(cancelled);
    let tool = Arc::new(BlockingTool {
        entered: AtomicUsize::new(0),
        release: Notify::new(),
    });
    let mut registry = ToolRegistry::new();
    registry.register_arc(tool.clone());
    let task = tokio::spawn(async move {
        registry
            .execute(
                "attacker-controlled-tool-name",
                serde_json::json!({}),
                &ToolCallContext::default(),
            )
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        while tool.entered.load(Ordering::Acquire) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("tool execution starts");
    task.abort();
    let _ = task.await;

    assert_eq!(sample_value(cancelled), before + 1);
    let mut body = String::new();
    crate::metrics::append_prometheus(&mut body);
    assert!(!body.contains("attacker-controlled-tool-name"));
}

#[tokio::test]
async fn missing_tool_is_a_bounded_error_outcome() {
    let errors = "ironcrew_tool_calls_total{outcome=\"error\"}";
    let before = sample_value(errors);
    let error = ToolRegistry::new()
        .execute(
            "secret-tool-that-does-not-exist",
            serde_json::json!({}),
            &ToolCallContext::default(),
        )
        .await
        .expect_err("unknown tool fails");
    assert!(error.to_string().contains("Tool not found"));
    assert_eq!(sample_value(errors), before + 1);
    let mut body = String::new();
    crate::metrics::append_prometheus(&mut body);
    assert!(!body.contains("secret-tool-that-does-not-exist"));
}
