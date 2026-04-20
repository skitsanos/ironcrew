//! `AgentAsTool` — wraps an agent so it can be invoked by another
//! agent's LLM tool-call loop. See
//! `docs/superpowers/specs/2026-04-20-agent-as-tool-design.md`.
//!
//! One instance is finalized per distinct `agent__<name>` reference
//! across the crew. The `Tool::execute` path:
//!
//! 1. parses the `prompt` argument,
//! 2. enforces the shared `IRONCREW_MAX_FLOW_DEPTH` cap (same counter
//!    as `run_flow`),
//! 3. emits `AgentToolStarted` / `AgentToolCompleted` bracket events,
//! 4. delegates a single turn to `run_single_agent_turn`, inheriting
//!    the caller's augmented tool registry so the sub-agent sees
//!    built-ins + MCP tools + sibling agent-tools.

use std::path::PathBuf;
use std::sync::{Arc, Weak};
use std::time::Instant;

use async_trait::async_trait;
use serde_json::Value;

use crate::engine::agent::Agent;
use crate::engine::eventbus::CrewEvent;
use crate::engine::runtime::Runtime;
use crate::llm::provider::{ChatMessage, LlmProvider, ToolSchema};
use crate::tools::{Tool, ToolCallContext};
use crate::utils::error::{IronCrewError, Result};

/// A `Tool` that delegates to an underlying `Agent`. Instances are
/// constructed during lazy crew finalization (one per distinct
/// `agent__<name>` reference) and registered alongside built-ins and
/// MCP tools so another agent's LLM can call them by name.
#[allow(dead_code)] // wired up in Tasks 9 + 10 (lazy finalization + registration)
pub struct AgentAsTool {
    /// Fully qualified tool name — always `agent__<agent.name>`.
    name: String,
    pub(crate) agent: Agent,
    provider: Arc<dyn LlmProvider>,
    /// Weak handle back to the owning `Runtime`. Used at call time to
    /// fall back to the runtime's default registry when the caller
    /// didn't supply one via `ToolCallContext`.
    runtime: Weak<Runtime>,
    /// Model id to use for this callee, already resolved through the
    /// model router (alias expansion, default fallback) at finalization.
    resolved_model: String,
    max_tool_rounds: usize,
    max_history: Option<usize>,
    /// Retained for future per-tool path scoping (e.g. file tools).
    #[allow(dead_code)]
    project_dir: Arc<PathBuf>,
    /// Pre-computed description so `Tool::description(&self) -> &str`
    /// can return a stable borrow without leaking memory.
    description_cached: String,
}

impl AgentAsTool {
    #[allow(dead_code, clippy::too_many_arguments)]
    // wired up in Tasks 9 + 10 (lazy finalization + registration)
    pub fn new(
        agent: Agent,
        provider: Arc<dyn LlmProvider>,
        runtime: Weak<Runtime>,
        resolved_model: String,
        max_tool_rounds: usize,
        max_history: Option<usize>,
        project_dir: Arc<PathBuf>,
    ) -> Self {
        let name = format!("agent__{}", agent.name);
        let description_cached = format!(
            "Delegate to specialist agent '{}'. Goal: {}. Use this tool when the \
             user asks for a task this specialist is best suited for.",
            agent.name, agent.goal
        );
        Self {
            name,
            agent,
            provider,
            runtime,
            resolved_model,
            max_tool_rounds,
            max_history,
            project_dir,
            description_cached,
        }
    }
}

#[async_trait]
impl Tool for AgentAsTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.description_cached
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: self.name.clone(),
            description: self.description_cached.clone(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "prompt": {
                        "type": "string",
                        "description": "The task for the specialist. Be specific about what you want them to produce."
                    }
                },
                "required": ["prompt"]
            }),
        }
    }

    async fn execute(&self, args: Value, ctx: &ToolCallContext) -> Result<String> {
        // 1. Parse `prompt` — return a user-visible error string (not an
        //    `Err`) so the caller LLM sees the validation failure as a
        //    regular tool result and can retry.
        let user_message = match args.get("prompt").and_then(Value::as_str) {
            Some(s) if !s.trim().is_empty() => s.to_string(),
            _ => return Ok("error: `prompt` is required (a non-empty string)".into()),
        };

        // 2. Shared depth cap with `run_flow`.
        let cap = crate::lua::subflow::max_flow_depth();
        if ctx.depth >= cap {
            return Ok(format!(
                "error: Agent delegation depth exceeded ({}). Simplify the pipeline \
                 or raise IRONCREW_MAX_FLOW_DEPTH.",
                cap
            ));
        }

        // 3. Prefer the caller's augmented registry (built-ins + MCP +
        //    sibling agent-tools); fall back to the runtime's default.
        let runtime = self.runtime.upgrade().ok_or_else(|| {
            IronCrewError::Validation("Agent-as-tool: parent Runtime has been dropped".into())
        })?;
        let augmented_registry = ctx
            .tool_registry
            .clone()
            .unwrap_or_else(|| runtime.tool_registry.clone());

        // 4. Build the sub-context. `caller_agent` becomes *this* agent
        //    so nested events attribute correctly; `caller_scope`
        //    propagates unchanged so the whole chain shares one scope.
        let sub_ctx = ToolCallContext {
            store: ctx.store.clone(),
            eventbus: ctx.eventbus.clone(),
            depth: ctx.depth + 1,
            tool_registry: Some(augmented_registry),
            caller_agent: Some(self.agent.name.clone()),
            caller_scope: ctx.caller_scope.clone(),
        };

        // 5. Emit the opening bracket event.
        let caller_name = ctx.caller_agent.clone().unwrap_or_default();
        if let Some(bus) = &ctx.eventbus {
            bus.emit(CrewEvent::AgentToolStarted {
                caller: caller_name.clone(),
                callee: self.agent.name.clone(),
                prompt: user_message.clone(),
            });
        }
        let started = Instant::now();

        // 6. Fresh history for this delegation — the sub-agent does not
        //    see the caller's conversation.
        let system_prompt =
            self.agent.system_prompt.clone().unwrap_or_else(|| {
                format!("You are {}. Goal: {}", self.agent.name, self.agent.goal)
            });
        let mut history = vec![
            ChatMessage::system(&system_prompt),
            ChatMessage::user(&user_message),
        ];

        let turn_result = crate::lua::agent_turn::run_single_agent_turn(
            &self.agent,
            &self.provider,
            &self.resolved_model,
            self.max_tool_rounds,
            self.max_history,
            &mut history,
            &sub_ctx,
        )
        .await;

        // 7. Closing bracket event — always emitted, with success flag.
        if let Some(bus) = &ctx.eventbus {
            bus.emit(CrewEvent::AgentToolCompleted {
                caller: caller_name,
                callee: self.agent.name.clone(),
                duration_ms: started.elapsed().as_millis() as u64,
                success: turn_result.is_ok(),
            });
        }

        // 8. Surface the assistant's final content or wrap the error
        //    with the tool name so the caller LLM sees structured
        //    attribution.
        match turn_result {
            Ok((content, _reasoning)) => Ok(content),
            Err(e) => Err(IronCrewError::ToolExecution {
                tool: self.name.clone(),
                message: e.to_string(),
            }),
        }
    }
}
