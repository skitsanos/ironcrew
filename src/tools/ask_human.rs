//! `ask_human` — agent-facing human-input tool.
//!
//! The flow-level counterpart is `crew:ask_human()` (a crew method scripted
//! by the flow author). This tool closes the other half of the loop: an
//! **agent decides mid-reasoning** that it needs the human, calls the tool,
//! and its turn suspends on the same per-run `InputBridge` — same events,
//! same `questions`/`answer` endpoints, same terminal prompt in CLI mode.
//!
//! Opt-in per agent (`tools = {"ask_human"}` in the agent definition), like
//! every other built-in.
//!
//! Timeout semantics differ from the crew method on purpose: a timed-out
//! question returns a **soft tool result** telling the model to proceed on
//! its best judgment, not a tool error — an error invites the model to
//! retry the call, which would park the run for another full timeout.

use async_trait::async_trait;

use crate::engine::eventbus::CrewEvent;
use crate::engine::input_bridge::{AskOutcome, default_timeout_secs};
use crate::engine::run_history::RunStatus;
use crate::llm::provider::ToolSchema;
use crate::tools::{Tool, ToolCallContext};
use crate::utils::error::{IronCrewError, Result};

/// Margin added on top of the question timeout when reporting a dispatch
/// deadline, so the bridge's own timeout always fires before the generic
/// tool-dispatch timeout kills the wait.
const DISPATCH_MARGIN_SECS: u64 = 10;

pub struct AskHumanTool;

impl AskHumanTool {
    pub fn new() -> Self {
        Self
    }
}

impl Default for AskHumanTool {
    fn default() -> Self {
        Self::new()
    }
}

fn parse_timeout(args: &serde_json::Value) -> u64 {
    args.get("timeout_s")
        .and_then(|v| v.as_u64())
        .unwrap_or_else(default_timeout_secs)
        .max(1)
}

#[async_trait]
impl Tool for AskHumanTool {
    fn name(&self) -> &str {
        "ask_human"
    }

    fn description(&self) -> &str {
        "Ask the human operator a question and wait for their answer. Use this when you \
         need information only the human has, or approval before a consequential step. \
         The run pauses until the human replies (or the timeout passes — then you must \
         proceed on your best judgment)."
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "ask_human".into(),
            description: self.description().into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "question": {
                        "type": "string",
                        "description": "The question to ask the human. Be specific and give enough context for them to answer without seeing your reasoning."
                    },
                    "choices": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Optional list of suggested answers, shown to the human as options. The human may still answer free-form."
                    },
                    "timeout_s": {
                        "type": "integer",
                        "description": "Optional seconds to wait before giving up (default 600)."
                    }
                },
                "required": ["question"]
            }),
        }
    }

    /// The bridge's own timeout must win the race against the generic
    /// tool-dispatch timeout (`IRONCREW_TOOL_TIMEOUT`, default 60 s) — a
    /// human legitimately takes minutes. Report question timeout + margin.
    fn dispatch_timeout(&self, args: &serde_json::Value) -> Option<std::time::Duration> {
        Some(std::time::Duration::from_secs(
            parse_timeout(args) + DISPATCH_MARGIN_SECS,
        ))
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolCallContext) -> Result<String> {
        let question = args
            .get("question")
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| IronCrewError::ToolExecution {
                tool: "ask_human".into(),
                message: "Missing 'question' argument".into(),
            })?;
        let choices: Vec<String> = args
            .get("choices")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        let timeout_s = parse_timeout(&args);

        let Some(ask) = &ctx.ask_human else {
            return Err(IronCrewError::ToolExecution {
                tool: "ask_human".into(),
                message: "Human input is not available in this execution context. \
                          Proceed without asking; do not retry this tool."
                    .into(),
            });
        };

        // Prefix the prompt with the asking agent so the human knows who is
        // talking when several agents (or the flow itself) ask questions.
        let prompt = match &ctx.caller_agent {
            Some(agent) => format!("[{}] {}", agent, question),
            None => question.to_string(),
        };

        let eventbus = ask.eventbus.clone().or_else(|| ctx.eventbus.clone());
        let store = ask.store.clone().or_else(|| ctx.store.clone());

        let question_id = uuid::Uuid::new_v4().to_string();
        if let Some(bus) = &eventbus {
            bus.emit(CrewEvent::HumanInputRequested {
                question_id: question_id.clone(),
                prompt: prompt.clone(),
                choices: choices.clone(),
                timeout_s,
                kind: "question".into(),
            });
        }

        // Best-effort status flip — same semantics as crew:ask_human.
        if let (Some(store), Some(run_id)) = (&store, &ask.run_id)
            && let Err(e) = store
                .update_run_status(run_id, RunStatus::WaitingForInput)
                .await
        {
            tracing::debug!("ask_human tool: run status not updated: {}", e);
        }

        let outcome = ask
            .bridge
            .ask(&question_id, &prompt, &choices, timeout_s, "question")
            .await?;

        if let (Some(store), Some(run_id)) = (&store, &ask.run_id)
            && ask.bridge.pending_count() == 0
            && let Err(e) = store.update_run_status(run_id, RunStatus::Running).await
        {
            tracing::debug!("ask_human tool: run status not restored: {}", e);
        }

        match outcome {
            AskOutcome::Answered(value) => {
                if let Some(bus) = &eventbus {
                    bus.emit(CrewEvent::HumanInputReceived {
                        question_id,
                        outcome: "answered".into(),
                    });
                }
                // Strings come back raw; structured answers as compact JSON —
                // either way the model receives plain text.
                Ok(match value {
                    serde_json::Value::String(s) => s,
                    other => other.to_string(),
                })
            }
            AskOutcome::TimedOut => {
                if let Some(bus) = &eventbus {
                    bus.emit(CrewEvent::HumanInputReceived {
                        question_id,
                        outcome: "timeout".into(),
                    });
                }
                // Soft result, not an error: an error tempts the model into
                // an immediate retry, which parks the run for another full
                // timeout window.
                Ok(format!(
                    "[no answer] The human did not respond within {}s. Do not call ask_human \
                     again for this question — proceed with your best judgment and note the \
                     assumption you made.",
                    timeout_s
                ))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::input_bridge::{AskHumanContext, BridgeMode, InputBridge};
    use std::sync::Arc;

    fn ctx_with_bridge() -> (ToolCallContext, Arc<InputBridge>) {
        let bridge = Arc::new(InputBridge::new(BridgeMode::Http));
        let ctx = ToolCallContext {
            caller_agent: Some("researcher".into()),
            ask_human: Some(AskHumanContext {
                bridge: bridge.clone(),
                run_id: None,
                store: None,
                eventbus: None,
            }),
            ..Default::default()
        };
        (ctx, bridge)
    }

    #[tokio::test]
    async fn answered_question_returns_answer_with_agent_prefix() {
        let (ctx, bridge) = ctx_with_bridge();
        let tool = Arc::new(AskHumanTool::new());

        let t = tool.clone();
        let exec = tokio::spawn(async move {
            t.execute(
                serde_json::json!({"question": "Which dataset?", "timeout_s": 30}),
                &ctx,
            )
            .await
        });

        while bridge.pending_count() == 0 {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        let q = &bridge.list()[0];
        // The human sees which agent is asking.
        assert_eq!(q.prompt, "[researcher] Which dataset?");
        bridge
            .answer(&q.question_id.clone(), serde_json::json!("the Q2 export"))
            .unwrap();

        assert_eq!(exec.await.unwrap().unwrap(), "the Q2 export");
    }

    #[tokio::test]
    async fn structured_answer_serializes_to_json_text() {
        let (ctx, bridge) = ctx_with_bridge();
        let tool = Arc::new(AskHumanTool::new());
        let t = tool.clone();
        let exec = tokio::spawn(async move {
            t.execute(
                serde_json::json!({"question": "Limits?", "timeout_s": 30}),
                &ctx,
            )
            .await
        });
        while bridge.pending_count() == 0 {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        let qid = bridge.list()[0].question_id.clone();
        bridge.answer(&qid, serde_json::json!({"max": 5})).unwrap();
        assert_eq!(exec.await.unwrap().unwrap(), r#"{"max":5}"#);
    }

    #[tokio::test]
    async fn timeout_returns_soft_result_not_error() {
        let (ctx, _bridge) = ctx_with_bridge();
        let tool = AskHumanTool::new();
        let out = tool
            .execute(
                serde_json::json!({"question": "Hello?", "timeout_s": 1}),
                &ctx,
            )
            .await
            .expect("timeout must be a soft Ok result");
        assert!(out.contains("[no answer]"), "got: {out}");
        assert!(out.contains("Do not call ask_human again"), "got: {out}");
    }

    #[tokio::test]
    async fn missing_bridge_errors_with_do_not_retry() {
        let ctx = ToolCallContext::default();
        let tool = AskHumanTool::new();
        let err = tool
            .execute(serde_json::json!({"question": "Hi"}), &ctx)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("not available"), "got: {err}");
    }

    #[tokio::test]
    async fn missing_question_is_an_error() {
        let (ctx, _bridge) = ctx_with_bridge();
        let tool = AskHumanTool::new();
        assert!(tool.execute(serde_json::json!({}), &ctx).await.is_err());
    }

    #[test]
    fn dispatch_timeout_exceeds_question_timeout() {
        let tool = AskHumanTool::new();
        let d = tool
            .dispatch_timeout(&serde_json::json!({"question": "x", "timeout_s": 300}))
            .unwrap();
        assert_eq!(d.as_secs(), 310);
    }
}
