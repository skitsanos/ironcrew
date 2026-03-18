pub mod file_read;
pub mod file_write;
pub mod hash;
pub mod http_request;
pub mod lua_tool;
pub mod registry;
pub mod shell;
pub mod template_render;
pub mod web_scrape;

use async_trait::async_trait;
use crate::utils::error::Result;
use crate::llm::provider::ToolSchema;

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn schema(&self) -> ToolSchema;
    async fn execute(&self, args: serde_json::Value) -> Result<String>;
}
