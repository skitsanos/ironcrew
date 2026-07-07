use std::collections::HashMap;
use std::sync::Arc;

use super::{Tool, ToolCallContext};
use crate::llm::provider::ToolSchema;
use crate::utils::error::Result;

#[derive(Clone)]
pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
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
