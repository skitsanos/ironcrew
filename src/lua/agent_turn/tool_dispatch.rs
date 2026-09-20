use std::time::{Duration, Instant};

use crate::engine::eventbus::CrewEvent;
use crate::llm::provider::ToolCallRequest;
use crate::tools::ToolCallContext;

pub(super) async fn execute(
    tool_call: &ToolCallRequest,
    scope: &str,
    ctx: &ToolCallContext,
) -> String {
    let started = Instant::now();
    if let Some(bus) = &ctx.eventbus {
        bus.emit(CrewEvent::ToolCall {
            task: scope.to_string(),
            tool: tool_call.function.name.clone(),
        });
    }

    let (result_text, success) =
        match crate::llm::tool_arguments::parse(&tool_call.function.arguments) {
            Ok(args) => dispatch(tool_call, args, ctx).await,
            Err(result) => {
                tracing::warn!(
                    scope,
                    tool = %tool_call.function.name,
                    "Provider returned malformed JSON tool arguments"
                );
                (result.to_string(), false)
            }
        };

    if let Some(bus) = &ctx.eventbus {
        bus.emit(CrewEvent::ToolResult {
            task: scope.to_string(),
            tool: tool_call.function.name.clone(),
            success,
            duration_ms: started.elapsed().as_millis() as u64,
        });
    }
    result_text
}

async fn dispatch(
    tool_call: &ToolCallRequest,
    args: serde_json::Value,
    ctx: &ToolCallContext,
) -> (String, bool) {
    let timeout = ctx.tool_registry.as_ref().map_or_else(
        || Duration::from_secs(super::tool_timeout_secs()),
        |registry| {
            registry
                .dispatch_timeout(&tool_call.function.name, &args)
                .unwrap_or_else(|| registry.default_dispatch_timeout())
        },
    );
    match &ctx.tool_registry {
        Some(registry) => {
            let dispatch = registry.execute(&tool_call.function.name, args, ctx);
            match tokio::time::timeout(timeout, dispatch).await {
                Ok(Ok(output)) => (output, true),
                Ok(Err(error)) => (format!("Tool error: {error}"), false),
                Err(_) => (
                    format!("Tool error: Tool timed out after {}s", timeout.as_secs()),
                    false,
                ),
            }
        }
        None => (
            format!(
                "Tool error: no tool registry available to dispatch {}",
                tool_call.function.name
            ),
            false,
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;

    use super::*;
    use crate::llm::provider::{ToolCallFunction, ToolSchema};
    use crate::tools::Tool;
    use crate::tools::registry::ToolRegistry;
    use crate::utils::error::Result;

    struct CountingTool(Arc<AtomicUsize>);

    #[async_trait]
    impl Tool for CountingTool {
        fn name(&self) -> &str {
            "counting"
        }

        fn description(&self) -> &str {
            "Counts executions"
        }

        fn schema(&self) -> ToolSchema {
            ToolSchema {
                name: self.name().into(),
                description: self.description().into(),
                parameters: serde_json::json!({"type": "object"}),
            }
        }

        async fn execute(
            &self,
            _args: serde_json::Value,
            _ctx: &ToolCallContext,
        ) -> Result<String> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok("executed".into())
        }
    }

    #[tokio::test]
    async fn malformed_arguments_do_not_reach_the_registry() {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut registry = ToolRegistry::new();
        registry.register(Box::new(CountingTool(Arc::clone(&calls))));
        let ctx = ToolCallContext {
            tool_registry: Some(registry),
            ..Default::default()
        };
        let tool_call = ToolCallRequest {
            id: "malformed".into(),
            call_type: "function".into(),
            function: ToolCallFunction {
                name: "counting".into(),
                arguments: r#"{"value":"not-closed""#.into(),
            },
        };

        let result = execute(&tool_call, "test-scope", &ctx).await;

        assert_eq!(
            result,
            crate::llm::tool_arguments::INVALID_TOOL_ARGUMENTS_RESULT
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }
}
