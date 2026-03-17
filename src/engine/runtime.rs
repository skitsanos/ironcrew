use std::path::Path;

use crate::llm::provider::LlmProvider;
use crate::tools::file_read::FileReadTool;
use crate::tools::file_write::FileWriteTool;
use crate::tools::registry::ToolRegistry;
use crate::tools::shell::ShellTool;
use crate::tools::web_scrape::WebScrapeTool;

pub struct Runtime {
    pub tool_registry: ToolRegistry,
    pub provider: Box<dyn LlmProvider>,
}

impl Runtime {
    pub fn new(provider: Box<dyn LlmProvider>, project_dir: Option<&Path>) -> Self {
        let mut tool_registry = ToolRegistry::new();

        let base_dir = project_dir.map(|p| p.to_path_buf());

        tool_registry.register(Box::new(FileReadTool::new(base_dir.clone())));
        tool_registry.register(Box::new(FileWriteTool::new(base_dir.clone(), None)));
        tool_registry.register(Box::new(WebScrapeTool::new(None)));
        // Shell tool intentionally NOT registered by default

        Self {
            tool_registry,
            provider,
        }
    }

    pub fn enable_shell_tool(&mut self) {
        self.tool_registry.register(Box::new(ShellTool::new()));
    }
}
