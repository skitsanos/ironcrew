use std::path::PathBuf;
use std::sync::Arc;

use mlua::{Lua, Result as LuaResult, Table};
use tokio::sync::Mutex;

use crate::engine::agent::Agent;
use crate::engine::crew::{Crew, ProviderConfig};
use crate::engine::memory::{MemoryConfig, MemoryStore};
use crate::engine::model_router::ModelRouter;
use crate::engine::runtime::Runtime;
use crate::llm::anthropic::{AnthropicConfig, AnthropicProvider, ServerTool};
use crate::llm::openai::OpenAiProvider;
use crate::llm::openai_responses::{
    OpenAiResponsesProvider, ResponsesConfig, ServerTool as ResponsesServerTool,
};
use crate::llm::provider::LlmProvider;
use crate::utils::error::IronCrewError;

// Re-export everything that was previously defined here so that existing
// import paths (`crate::lua::api::…`) continue to work unchanged.
// Some re-exports are only consumed by integration tests or downstream crates.
#[allow(unused_imports)]
pub use super::crew_userdata::LuaCrew;
#[allow(unused_imports)]
pub use super::json::{json_value_to_lua, lua_table_to_json, lua_value_to_json};
#[allow(unused_imports)]
pub use super::parsers::{
    LuaToolDef, agent_from_lua_table, load_agents_from_files, load_tool_defs_from_files,
    task_from_lua_table, tool_def_from_lua_table,
};

// ---------------------------------------------------------------------------
// Global registrations
// ---------------------------------------------------------------------------

/// Register the env() global function in Lua.
#[allow(dead_code)] // used in integration tests
pub fn register_env_function(lua: &Lua) -> LuaResult<()> {
    let env_fn = lua.create_function(|_, name: String| Ok(std::env::var(&name).ok()))?;
    lua.globals().set("env", env_fn)?;
    Ok(())
}

/// Register Agent.new() constructor in Lua.
/// Validates the table and returns it back (so crew:add_agent() receives a table).
pub fn register_agent_constructor(lua: &Lua) -> LuaResult<()> {
    let agent_table = lua.create_table()?;

    let new_fn = lua.create_function(|_, table: Table| {
        // Validate the table has required fields
        agent_from_lua_table(&table)?;
        // Return the original table (not a serialized string)
        Ok(table)
    })?;

    agent_table.set("new", new_fn)?;
    lua.globals().set("Agent", agent_table)?;
    Ok(())
}

/// Register Crew.new() constructor. Requires provider setup.
/// Preloaded agents (from agents/ directory) are auto-injected into every new Crew.
pub fn register_crew_constructor(
    lua: &Lua,
    runtime: Arc<Runtime>,
    preloaded_agents: Vec<Agent>,
    project_dir: PathBuf,
) -> LuaResult<()> {
    let crew_table = lua.create_table()?;
    let agents = Arc::new(preloaded_agents);
    let project_dir = Arc::new(project_dir);

    let new_fn = lua.create_function(move |_, table: Table| {
        let project_dir = (*project_dir).clone();
        let goal: String = table.get("goal")?;
        let provider: String = table
            .get::<String>("provider")
            .unwrap_or_else(|_| "openai".into());
        let model: String = table
            .get::<String>("model")
            .unwrap_or_else(|_| "gpt-4.1-mini".into());
        let base_url: Option<String> = table.get("base_url").ok();
        let api_key: Option<String> = table.get("api_key").ok();
        let max_concurrent: Option<usize> =
            table.get::<Option<usize>>("max_concurrent").ok().flatten();
        let normalized_provider = provider.to_lowercase();

        if !matches!(
            normalized_provider.as_str(),
            "openai" | "anthropic" | "openai-responses"
        ) {
            return Err(mlua::Error::external(IronCrewError::Validation(format!(
                "Unsupported provider '{}'. Supported: 'openai', 'anthropic', 'openai-responses'.",
                provider
            ))));
        }

        // Create a custom provider based on provider type
        let custom_provider: Option<Arc<dyn LlmProvider>> =
            if normalized_provider == "anthropic" {
                // Anthropic always creates a dedicated provider
                let key = api_key
                    .clone()
                    .or_else(|| std::env::var("ANTHROPIC_API_KEY").ok())
                    .filter(|k| !k.trim().is_empty())
                    .ok_or_else(|| {
                        mlua::Error::external(IronCrewError::Validation(
                            "Anthropic provider requires an api_key or ANTHROPIC_API_KEY env var"
                                .to_string(),
                        ))
                    })?;

                // Parse Anthropic-specific config
                let thinking_budget: Option<u32> = table.get("thinking_budget").ok();

                let server_tools_list: Vec<String> = table
                    .get::<mlua::Table>("server_tools")
                    .map(|t| {
                        t.sequence_values::<String>()
                            .filter_map(|v| v.ok())
                            .collect()
                    })
                    .unwrap_or_default();

                let web_search_max_uses: Option<u32> = table.get("web_search_max_uses").ok();

                let server_tools: Vec<ServerTool> = server_tools_list
                    .iter()
                    .filter_map(|name| match name.as_str() {
                        "web_search" => Some(ServerTool::WebSearch {
                            max_uses: web_search_max_uses,
                        }),
                        "code_execution" => Some(ServerTool::CodeExecution),
                        other => {
                            tracing::warn!("Unknown Anthropic server tool: '{}'", other);
                            None
                        }
                    })
                    .collect();

                let anthropic_config = AnthropicConfig {
                    thinking_budget,
                    server_tools,
                };

                Some(Arc::new(AnthropicProvider::new(
                    key,
                    base_url.clone(),
                    anthropic_config,
                )))
            } else if normalized_provider == "openai-responses" {
                // OpenAI Responses API (also supports Azure, xAI/Grok, OpenRouter)
                let key = api_key
                    .clone()
                    .or_else(|| {
                        if let Some(ref url) = base_url
                            && url.contains("x.ai")
                        {
                            return std::env::var("XAI_API_KEY").ok();
                        }
                        None
                    })
                    .or_else(|| std::env::var("OPENAI_API_KEY").ok())
                    .filter(|k| !k.trim().is_empty())
                    .ok_or_else(|| {
                        mlua::Error::external(IronCrewError::Validation(
                            "openai-responses provider requires an api_key or OPENAI_API_KEY env var"
                                .to_string(),
                        ))
                    })?;

                // Parse Responses-specific config
                let reasoning_effort: Option<String> = table.get("reasoning_effort").ok();
                let reasoning_summary: Option<String> = table.get("reasoning_summary").ok();

                let server_tools_list: Vec<String> = table
                    .get::<mlua::Table>("server_tools")
                    .map(|t| {
                        t.sequence_values::<String>()
                            .filter_map(|v| v.ok())
                            .collect()
                    })
                    .unwrap_or_default();

                let file_search_vector_store_ids: Vec<String> = table
                    .get::<mlua::Table>("file_search_vector_store_ids")
                    .map(|t| {
                        t.sequence_values::<String>()
                            .filter_map(|v| v.ok())
                            .collect()
                    })
                    .unwrap_or_default();

                let file_search_max_results: Option<u32> =
                    table.get("file_search_max_results").ok();

                let web_search_context_size: Option<String> =
                    table.get("web_search_context_size").ok();

                let server_tools: Vec<ResponsesServerTool> = server_tools_list
                    .iter()
                    .filter_map(|name| match name.as_str() {
                        "web_search" => Some(ResponsesServerTool::WebSearch {
                            context_size: web_search_context_size.clone(),
                        }),
                        "file_search" => Some(ResponsesServerTool::FileSearch {
                            vector_store_ids: file_search_vector_store_ids.clone(),
                            max_num_results: file_search_max_results,
                        }),
                        "code_interpreter" => Some(ResponsesServerTool::CodeInterpreter),
                        other => {
                            tracing::warn!("Unknown Responses server tool: '{}'", other);
                            None
                        }
                    })
                    .collect();

                let responses_config = ResponsesConfig {
                    reasoning_effort,
                    reasoning_summary,
                    server_tools,
                };

                Some(Arc::new(OpenAiResponsesProvider::new(
                    key,
                    base_url.clone(),
                    responses_config,
                )))
            } else if api_key.is_some() || base_url.is_some() {
                // OpenAI with custom settings
                // Resolve API key: explicit > provider-specific env var > OPENAI_API_KEY
                let key = match api_key
                    .clone()
                    .or_else(|| {
                        if let Some(ref url) = base_url {
                            if url.contains("generativelanguage.googleapis.com")
                                || url.contains("gemini")
                            {
                                return std::env::var("GEMINI_API_KEY").ok();
                            }
                            if url.contains("groq.com") {
                                return std::env::var("GROQ_API_KEY").ok();
                            }
                            if url.contains("anthropic.com") {
                                return std::env::var("ANTHROPIC_API_KEY").ok();
                            }
                        }
                        None
                    })
                    .or_else(|| std::env::var("OPENAI_API_KEY").ok())
                    .filter(|k| !k.trim().is_empty())
                {
                    Some(key) => key,
                    None => {
                        return Err(mlua::Error::external(IronCrewError::Validation(
                            "Crew with custom provider settings requires an api_key (set via env var or Crew.new config)".to_string(),
                        )));
                    }
                };
                let url = base_url.clone();
                Some(Arc::new(OpenAiProvider::new(key, url)))
            } else {
                None
            };

        let config = ProviderConfig {
            provider,
            model,
            base_url,
            api_key,
        };

        let memory_mode: String = table
            .get::<String>("memory")
            .unwrap_or_else(|_| "ephemeral".into());

        let defaults = MemoryConfig::default();
        let memory_config = MemoryConfig {
            max_items: table
                .get::<Option<usize>>("max_memory_items")
                .ok()
                .flatten()
                .or(defaults.max_items),
            max_total_tokens: table
                .get::<Option<usize>>("max_memory_tokens")
                .ok()
                .flatten()
                .or(defaults.max_total_tokens),
        };

        let memory = match memory_mode.as_str() {
            "persistent" => {
                let memory_path = project_dir.join(".ironcrew").join("memory.json");
                MemoryStore::persistent_with_config(memory_path, memory_config)
                    .map_err(mlua::Error::external)?
            }
            _ => MemoryStore::ephemeral_with_config(memory_config),
        };

        let stream: bool = table.get::<bool>("stream").unwrap_or(false);

        let model_router = if let Ok(models_table) = table.get::<mlua::Table>("models") {
            let mut router = ModelRouter::new();
            for (purpose, model_name) in models_table.pairs::<String, String>().flatten() {
                router.set(&purpose, model_name);
            }
            router
        } else {
            ModelRouter::new()
        };

        let prompt_cache_key: Option<String> = table.get("prompt_cache_key").ok();
        let prompt_cache_retention: Option<String> = table.get("prompt_cache_retention").ok();

        let mut crew = Crew::new(goal, config, memory);
        crew.max_concurrent_tasks = max_concurrent;
        crew.stream = stream;
        crew.model_router = model_router;
        crew.prompt_cache_key = prompt_cache_key;
        crew.prompt_cache_retention = prompt_cache_retention;

        // Auto-inject preloaded agents from agents/ directory
        for agent in agents.iter() {
            crew.add_agent(agent.clone());
        }

        Ok(LuaCrew {
            crew: Arc::new(Mutex::new(crew)),
            runtime: runtime.clone(),
            custom_provider,
            project_dir,
        })
    })?;

    crew_table.set("new", new_fn)?;
    lua.globals().set("Crew", crew_table)?;
    Ok(())
}
