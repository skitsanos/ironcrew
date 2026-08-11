use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock, Weak};

use crate::engine::conversation_definition::ConversationSourceContext;
use crate::llm::provider::LlmProvider;
use crate::tools::file_read::FileReadTool;
use crate::tools::file_read_glob::FileReadGlobTool;
use crate::tools::file_write::FileWriteTool;
use crate::tools::hash::HashTool;
use crate::tools::http_request::HttpRequestTool;
use crate::tools::lua_tool::LuaScriptTool;
use crate::tools::registry::ToolRegistry;
use crate::tools::runtime_policy::{LuaVmPolicy, RuntimeExecutionPolicy};
use crate::tools::shell::ShellTool;
use crate::tools::template_render::TemplateRenderTool;
use crate::tools::validate_schema::ValidateSchemaTool;
use crate::tools::web_scrape::WebScrapeTool;
use crate::utils::error::Result;

pub struct Runtime {
    pub tool_registry: ToolRegistry,
    pub provider: Arc<dyn LlmProvider>,
    project_dir: Option<PathBuf>,
    write_dir: Option<PathBuf>,
    conversation_source: Option<ConversationSourceContext>,
    /// Strong `Arc`s to every registered `LuaScriptTool`. Kept in parallel
    /// with the trait-object registry so `set_self_ref` can hand each tool
    /// its weak runtime reference without `Any`-downcasting.
    lua_tools: Vec<Arc<LuaScriptTool>>,
    /// Weak self-reference. Set after the `Runtime` is wrapped in `Arc`.
    /// Exposed via `upgrade_self` for consumers that need a strong handle
    /// back from inside a bare `&Runtime` method.
    self_ref: OnceLock<Weak<Runtime>>,
}

impl Runtime {
    #[allow(dead_code)] // public constructor used by embedders and integration tests
    pub fn new(provider: Box<dyn LlmProvider>, project_dir: Option<&Path>) -> Self {
        Self::new_with_conversation_source(provider, project_dir, None)
    }

    pub fn new_with_conversation_source(
        provider: Box<dyn LlmProvider>,
        project_dir: Option<&Path>,
        conversation_source: Option<ConversationSourceContext>,
    ) -> Self {
        Self::new_with_conversation_source_and_execution_policy(
            provider,
            project_dir,
            conversation_source,
            RuntimeExecutionPolicy::capture(),
        )
    }

    pub(crate) fn new_with_conversation_source_and_execution_policy(
        provider: Box<dyn LlmProvider>,
        project_dir: Option<&Path>,
        conversation_source: Option<ConversationSourceContext>,
        execution_policy: RuntimeExecutionPolicy,
    ) -> Self {
        let execution_policy = if conversation_source.is_some() {
            execution_policy.block_persistent_lua_env()
        } else {
            execution_policy
        };
        let http_policy = execution_policy.lua_http_policy();
        let mut tool_registry = ToolRegistry::with_execution_policy(execution_policy);

        let base_dir = project_dir.map(|p| p.to_path_buf());
        let write_base_dir = std::env::var_os("IRONCREW_FILE_WRITE_ROOT")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .or_else(|| base_dir.clone());

        tool_registry.register(Box::new(FileReadTool::new(base_dir.clone())));
        tool_registry.register(Box::new(FileReadGlobTool::new(base_dir.clone())));
        tool_registry.register(Box::new(FileWriteTool::new(write_base_dir.clone(), None)));
        tool_registry.register(Box::new(WebScrapeTool::new(None)));
        tool_registry.register(Box::new(HttpRequestTool::with_policy(http_policy)));
        tool_registry.register(Box::new(HashTool::new()));
        tool_registry.register(Box::new(TemplateRenderTool::new()));
        tool_registry.register(Box::new(ValidateSchemaTool::new()));
        // Agent-facing human-input tool. Registered unconditionally (agents
        // still opt in via their tools list); without a per-run bridge the
        // tool fails with a clear "unavailable" message instead of hanging.
        tool_registry.register(Box::new(crate::tools::ask_human::AskHumanTool::new()));

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
            provider: crate::llm::metrics::observe_boxed_provider(provider),
            project_dir: base_dir,
            write_dir: write_base_dir,
            conversation_source,
            lua_tools: Vec::new(),
            self_ref: OnceLock::new(),
        }
    }

    #[allow(dead_code)] // part of public API
    pub fn enable_shell_tool(&mut self) {
        self.tool_registry.register(Box::new(ShellTool::new()));
    }

    /// Register Lua-defined tools from metadata and the exact source bytes that
    /// produced it. The source path is diagnostic only and is never reopened.
    pub fn register_lua_tools(
        &mut self,
        tool_defs: Vec<crate::lua::api::LuaToolDef>,
    ) -> Result<()> {
        let vm_policy = self.tool_registry.lua_vm_policy()?;
        for def in tool_defs {
            let lua_tool = Arc::new(LuaScriptTool::new(
                def.name,
                def.description,
                def.parameters,
                def.source,
                (self.project_dir.clone(), self.write_dir.clone()),
                vm_policy.clone(),
                self.conversation_source.clone(),
            ));
            let as_tool: Arc<dyn crate::tools::Tool> = lua_tool.clone();
            self.tool_registry.register_arc(as_tool);
            self.lua_tools.push(lua_tool);
        }
        Ok(())
    }

    /// Store the weak self-reference and propagate it (plus the shared
    /// project-directory `Arc`) to every registered `LuaScriptTool`. Called
    /// from `setup_crew_runtime` right after `Arc::new(runtime)`.
    pub fn set_self_ref(&self, weak: Weak<Runtime>) {
        let _ = self.self_ref.set(weak.clone());

        let project_dir_arc = self.project_dir.as_ref().map(|p| Arc::new(p.clone()));

        for lua_tool in &self.lua_tools {
            lua_tool.set_runtime(weak.clone());
            if let Some(ref dir) = project_dir_arc {
                lua_tool.set_project_dir(dir.clone());
            }
        }
    }

    pub(crate) fn lua_vm_policy(&self) -> Result<LuaVmPolicy> {
        self.tool_registry.lua_vm_policy()
    }

    /// Upgrade the stored weak self-reference to a strong `Arc<Runtime>`.
    /// Returns `None` if `set_self_ref` was never called or the owning
    /// `Arc` has already been dropped.
    #[allow(dead_code)] // exposed for future consumers
    pub fn upgrade_self(&self) -> Option<Arc<Runtime>> {
        self.self_ref.get().and_then(|w| w.upgrade())
    }
}
