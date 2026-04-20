pub mod file_read;
pub mod file_read_glob;
pub mod file_write;
pub mod hash;
pub mod http_request;
pub mod lua_tool;
pub mod registry;
pub mod shell;
pub mod template_render;
pub mod validate_schema;
pub mod web_scrape;

use std::sync::Arc;

use crate::engine::eventbus::EventBus;
use crate::engine::store::StateStore;
use crate::llm::provider::ToolSchema;
use crate::utils::error::Result;
use async_trait::async_trait;

/// Runtime context passed to every `Tool::execute` call. Fields are
/// populated by the caller and read by tools that need them; most
/// built-in tools ignore the whole thing.
#[derive(Default, Clone)]
pub struct ToolCallContext {
    /// Persistent session store (conversation/dialog records). `None`
    /// in CLI one-shot paths that don't need persistence.
    pub store: Option<Arc<dyn StateStore>>,

    /// Event bus for telemetry. `None` in test contexts where events
    /// aren't observed.
    pub eventbus: Option<EventBus>,

    /// Sub-flow / agent-tool nesting depth. Checked against
    /// `IRONCREW_MAX_FLOW_DEPTH` by delegation primitives.
    pub depth: usize,

    /// Caller's augmented tool registry (built-ins + MCP tools + agent-tools).
    /// Agent-as-tool invocations read this so the sub-agent inherits
    /// the caller's tool view. `None` in admin/CLI-run paths that haven't
    /// built an augmented view.
    pub tool_registry: Option<registry::ToolRegistry>,

    /// Name of the agent whose tool-call loop triggered this dispatch.
    /// Used by `AgentAsTool` to attribute `AgentToolStarted` /
    /// `AgentToolCompleted` events. `None` when the caller is a
    /// top-level script outside any agent loop.
    pub caller_agent: Option<String>,

    /// Correlation identifier for the outer orchestration context
    /// (task name, conversation id, dialog id, etc.). Used as the
    /// `task` field on nested `ToolCall` / `ToolResult` / `TaskThinking`
    /// events so sub-agent activity is attributable to the parent.
    /// Propagated unchanged through agent-as-tool nesting.
    pub caller_scope: Option<String>,
}

impl std::fmt::Debug for ToolCallContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolCallContext")
            .field("store", &self.store.as_ref().map(|_| "<StateStore>"))
            .field("eventbus", &self.eventbus.as_ref().map(|_| "<EventBus>"))
            .field("depth", &self.depth)
            .field(
                "tool_registry",
                &self.tool_registry.as_ref().map(|_| "<ToolRegistry>"),
            )
            .field("caller_agent", &self.caller_agent)
            .field("caller_scope", &self.caller_scope)
            .finish()
    }
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn schema(&self) -> ToolSchema;
    async fn execute(&self, args: serde_json::Value, ctx: &ToolCallContext) -> Result<String>;
}
