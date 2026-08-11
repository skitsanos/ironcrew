use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;

use super::{Tool, ToolCallContext};
use crate::llm::provider::ToolSchema;
use crate::utils::error::Result;

#[derive(Clone)]
pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
    execution_policy: crate::tools::runtime_policy::RuntimeExecutionPolicy,
    /// Human sign-off policy for gated tools. `None` = no gate (the
    /// common case, zero dispatch overhead). Clones share the policy's
    /// grant set via `Arc`, so "always" grants follow the augmented
    /// registries handed to delegated sub-agents.
    approval: Option<crate::tools::approval::ApprovalPolicy>,
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self {
            tools: HashMap::new(),
            execution_policy: crate::tools::runtime_policy::RuntimeExecutionPolicy::capture(),
            approval: None,
        }
    }

    pub(crate) fn with_execution_policy(
        execution_policy: crate::tools::runtime_policy::RuntimeExecutionPolicy,
    ) -> Self {
        Self {
            tools: HashMap::new(),
            execution_policy,
            approval: None,
        }
    }

    /// Attach (or clear) the approval policy. Called once per crew at
    /// agent-tool finalization; augmented clones inherit it.
    pub fn set_approval_policy(&mut self, policy: Option<crate::tools::approval::ApprovalPolicy>) {
        self.approval = policy;
    }

    pub fn register(&mut self, tool: Box<dyn Tool>) {
        self.tools.insert(tool.name().to_string(), Arc::from(tool));
    }

    /// Register a tool that's already wrapped in `Arc`. Lets the caller
    /// keep a second strong reference to the same instance (e.g. so
    /// `Runtime::set_self_ref` can reach back into `LuaScriptTool`s to
    /// populate their weak runtime refs without `Any`-downcasting).
    pub fn register_arc(&mut self, tool: Arc<dyn Tool>) {
        self.tools.insert(tool.name().to_string(), tool);
    }

    pub fn get(&self, name: &str) -> Option<&dyn Tool> {
        self.tools.get(name).map(|t| t.as_ref())
    }

    pub fn list(&self) -> Vec<String> {
        self.tools.keys().cloned().collect()
    }

    #[allow(dead_code)] // used in integration tests
    pub fn schemas(&self) -> Vec<ToolSchema> {
        self.tools.values().map(|t| t.schema()).collect()
    }

    pub fn schemas_for(&self, tool_names: &[String]) -> Vec<ToolSchema> {
        tool_names
            .iter()
            .filter_map(|name| self.tools.get(name).map(|t| t.schema()))
            .collect()
    }

    /// Canonical identity of the exact ordered tool graph available to one
    /// conversation agent. Unlike `schemas_for`, this fails closed if a named
    /// tool is unavailable so replicas cannot silently construct different
    /// provider requests under the same durable definition.
    pub fn conversation_execution_fingerprint(&self, tool_names: &[String]) -> Result<String> {
        let mut visited = HashSet::new();
        let mut definitions = Vec::new();
        for name in tool_names {
            self.collect_conversation_definition(name, &mut visited, &mut definitions)?;
        }
        crate::engine::conversation_provider::resolved_tools_fingerprint(&serde_json::json!({
            "global_execution_policy": self.execution_policy.definition()?,
            "tools": definitions,
        }))
    }

    pub(crate) fn default_dispatch_timeout(&self) -> std::time::Duration {
        self.execution_policy.tool_timeout()
    }

    pub(crate) fn max_flow_depth(&self) -> usize {
        self.execution_policy.max_flow_depth()
    }

    pub(crate) fn lua_vm_policy(&self) -> Result<crate::tools::runtime_policy::LuaVmPolicy> {
        self.execution_policy.lua_vm_policy()
    }

    pub(crate) fn max_reasoning_bytes(&self) -> usize {
        self.execution_policy.max_reasoning_bytes()
    }

    pub(crate) fn chat_history_max_bytes(&self) -> usize {
        self.execution_policy.chat_history_max_bytes()
    }

    pub(crate) fn conversation_policy(
        &self,
    ) -> crate::tools::conversation_policy::ConversationTurnPolicy {
        self.execution_policy.conversation_policy()
    }

    pub(crate) fn conversation_turn_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.conversation_policy().turn_timeout_secs())
    }

    fn collect_conversation_definition(
        &self,
        name: &str,
        visited: &mut HashSet<String>,
        definitions: &mut Vec<serde_json::Value>,
    ) -> Result<()> {
        if !visited.insert(name.to_string()) {
            return Ok(());
        }
        let tool = self.tools.get(name).ok_or_else(|| {
            crate::utils::error::IronCrewError::Validation(format!(
                "Conversation tool '{name}' is not registered"
            ))
        })?;
        let approval = self
            .approval
            .as_ref()
            .filter(|policy| policy.requires(name))
            .map(crate::tools::approval::ApprovalPolicy::conversation_definition);
        definitions.push(serde_json::json!({
            "name": name,
            "approval": approval,
            "definition": tool.conversation_definition()?,
        }));
        for dependency in tool.conversation_dependencies() {
            self.collect_conversation_definition(&dependency, visited, definitions)?;
        }
        Ok(())
    }

    /// Effective dispatch deadline for one call: the tool's own override
    /// (e.g. `ask_human` waiting on a person), further extended when the
    /// call is gated-and-not-yet-granted so the generic tool timeout can't
    /// kill a legitimate approval wait. `None` = use the global default.
    pub fn dispatch_timeout(
        &self,
        name: &str,
        args: &serde_json::Value,
    ) -> Option<std::time::Duration> {
        let own = self.get(name).and_then(|t| t.dispatch_timeout(args));
        let gate = self
            .approval
            .as_ref()
            .and_then(|p| crate::tools::approval::gate_dispatch_allowance(p, name));
        match (own, gate) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        }
    }

    pub async fn execute(
        &self,
        name: &str,
        args: serde_json::Value,
        ctx: &ToolCallContext,
    ) -> Result<String> {
        let tool = self.tools.get(name).ok_or_else(|| {
            crate::utils::error::IronCrewError::ToolExecution {
                tool: name.to_string(),
                message: "Tool not found".into(),
            }
        })?;

        // Approval gate: a gated, not-yet-granted call needs a human allow
        // before the tool runs. Fail closed — deny on timeout, missing
        // bridge, or any answer that isn't an explicit allow token.
        if let Some(policy) = &self.approval
            && policy.requires(name)
            && !policy.is_granted(name)
        {
            match crate::tools::approval::request(name, &args, ctx, policy).await? {
                crate::tools::approval::Verdict::Allow => {}
                crate::tools::approval::Verdict::Deny(reason) => {
                    return Err(crate::utils::error::IronCrewError::ToolExecution {
                        tool: name.to_string(),
                        message: reason,
                    });
                }
            }
        }

        tool.execute(args, ctx).await
    }
}

#[cfg(test)]
mod tests;
