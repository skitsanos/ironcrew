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

/// Context handed to every `Tool::execute` invocation.
///
/// Primarily used by `LuaScriptTool` to seed the per-call Lua VM's app-data
/// so that sandbox-level primitives like `run_flow()` can reach the runtime,
/// the shared store, and the current subflow depth. Built-in tools ignore
/// every field — the parameter is kept as `&ToolCallContext` (not optional)
/// so call sites make the intent explicit.
#[derive(Clone, Default)]
pub struct ToolCallContext {
    /// Shared state store (if the caller has one instantiated).
    pub store: Option<Arc<dyn StateStore>>,
    /// EventBus the caller wants log/telemetry events routed through.
    pub eventbus: Option<EventBus>,
    /// Current subflow nesting depth. Sub-flows spawned from the tool inherit
    /// `depth + 1`. Root callers pass `0`.
    pub depth: usize,
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn schema(&self) -> ToolSchema;
    async fn execute(&self, args: serde_json::Value, ctx: &ToolCallContext) -> Result<String>;
}
