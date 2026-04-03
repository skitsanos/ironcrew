use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::llm::provider::LlmProvider;
use crate::tools::file_read::FileReadTool;
use crate::tools::file_read_glob::FileReadGlobTool;
use crate::tools::file_write::FileWriteTool;
use crate::tools::hash::HashTool;
use crate::tools::http_request::HttpRequestTool;
use crate::tools::registry::ToolRegistry;
use crate::tools::shell::ShellTool;
use crate::tools::template_render::TemplateRenderTool;
use crate::tools::validate_schema::ValidateSchemaTool;
use crate::tools::web_scrape::WebScrapeTool;
use crate::utils::error::{IronCrewError, Result};

pub struct Runtime {
    pub tool_registry: ToolRegistry,
    pub provider: Arc<dyn LlmProvider>,
    project_dir: Option<PathBuf>,
}

impl Runtime {
    pub fn new(provider: Box<dyn LlmProvider>, project_dir: Option<&Path>) -> Self {
        let mut tool_registry = ToolRegistry::new();

        let base_dir = project_dir.map(|p| p.to_path_buf());

        tool_registry.register(Box::new(FileReadTool::new(base_dir.clone())));
        tool_registry.register(Box::new(FileReadGlobTool::new(base_dir.clone())));
        tool_registry.register(Box::new(FileWriteTool::new(base_dir.clone(), None)));
        tool_registry.register(Box::new(WebScrapeTool::new(None)));
        tool_registry.register(Box::new(HttpRequestTool::new()));
        tool_registry.register(Box::new(HashTool::new()));
        tool_registry.register(Box::new(TemplateRenderTool::new()));
        tool_registry.register(Box::new(ValidateSchemaTool::new()));

        // Shell tool only registered when explicitly opted in via env var
        if std::env::var("IRONCREW_ALLOW_SHELL")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false)
        {
            tracing::warn!(
                "Shell tool enabled via IRONCREW_ALLOW_SHELL — agents can execute arbitrary commands"
            );
            tool_registry.register(Box::new(ShellTool::new()));
        }

        Self {
            tool_registry,
            provider: Arc::from(provider),
            project_dir: base_dir,
        }
    }

    #[allow(dead_code)] // part of public API
    pub fn enable_shell_tool(&mut self) {
        self.tool_registry.register(Box::new(ShellTool::new()));
    }

    /// Register Lua-defined tools from tool definition metadata.
    /// Reads source from each tool's file path and wraps it in a LuaScriptTool.
    pub fn register_lua_tools(
        &mut self,
        tool_defs: Vec<crate::lua::api::LuaToolDef>,
    ) -> Result<()> {
        for def in tool_defs {
            let source = std::fs::read_to_string(&def.source_path).map_err(|err| {
                IronCrewError::Validation(format!(
                    "Failed to read Lua tool source '{}': {}",
                    def.name, err
                ))
            })?;
            let lua_tool = crate::tools::lua_tool::LuaScriptTool::new(
                def.name,
                def.description,
                def.parameters,
                source,
                self.project_dir.clone(),
            );
            self.tool_registry.register(Box::new(lua_tool));
        }
        Ok(())
    }
}
