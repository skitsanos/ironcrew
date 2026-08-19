use std::path::PathBuf;
use std::sync::Arc;

use mlua::{Lua, Result as LuaResult, Table, Value};
use tokio::sync::Mutex;

use crate::engine::agent::Agent;
use crate::engine::crew::{Crew, ProviderConfig};
use crate::engine::memory::{MemoryConfig, MemoryStore};
use crate::engine::model_router::ModelRouter;
use crate::engine::runtime::Runtime;
use crate::engine::store::StateStore;
use crate::llm::anthropic::{AnthropicConfig, AnthropicProvider, ServerTool};
use crate::llm::openai::OpenAiProvider;
use crate::llm::openai_responses::{
    OpenAiResponsesProvider, ResponsesConfig, ServerTool as ResponsesServerTool,
};
use crate::llm::provider::LlmProvider;
use crate::utils::error::IronCrewError;

#[cfg(feature = "mcp")]
use crate::mcp::parse_mcp_config;

const MAX_GOAL_BYTES: usize = 1024 * 1024;
const MAX_PROVIDER_NAME_BYTES: usize = 128;
const MAX_MODEL_NAME_BYTES: usize = 1024;
const MAX_API_KEY_BYTES: usize = 16 * 1024;
const MAX_CONFIG_ITEM_BYTES: usize = 4096;
const DEFAULT_MAX_MEMORY_ITEMS: usize = 10_000;
const HARD_MAX_MEMORY_ITEMS: usize = 100_000;
const DEFAULT_MAX_MEMORY_TOKENS: usize = 1_000_000;
const HARD_MAX_MEMORY_TOKENS: usize = 10_000_000;
const DEFAULT_MAX_SERVER_TOOLS: usize = 16;
const HARD_MAX_SERVER_TOOLS: usize = 64;
const DEFAULT_MAX_VECTOR_STORE_IDS: usize = 32;
const HARD_MAX_VECTOR_STORE_IDS: usize = 256;
const DEFAULT_MAX_MODEL_ROUTES: usize = 64;
const HARD_MAX_MODEL_ROUTES: usize = 256;
const MAX_WEB_SEARCH_USES: u32 = 100;
const MAX_FILE_SEARCH_RESULTS: u32 = 1_000;
const MAX_THINKING_BUDGET: u32 = 1_000_000;

fn config_limit(name: &str, default: usize, hard_max: usize) -> LuaResult<usize> {
    match std::env::var(name) {
        Ok(raw) => {
            let value = raw.parse::<usize>().map_err(|_| {
                mlua::Error::external(IronCrewError::Validation(format!(
                    "{name} must be an integer between 1 and {hard_max}"
                )))
            })?;
            if value == 0 || value > hard_max {
                return Err(mlua::Error::external(IronCrewError::Validation(format!(
                    "{name} must be between 1 and {hard_max}; got {value}"
                ))));
            }
            Ok(value)
        }
        Err(std::env::VarError::NotPresent) => Ok(default),
        Err(std::env::VarError::NotUnicode(_)) => Err(mlua::Error::external(
            IronCrewError::Validation(format!("{name} must contain valid UTF-8")),
        )),
    }
}

fn validate_config_string(field: &str, value: &str, max_bytes: usize) -> LuaResult<()> {
    if value.trim().is_empty() {
        return Err(mlua::Error::external(IronCrewError::Validation(format!(
            "Crew.new {field} must not be empty"
        ))));
    }
    if value.len() > max_bytes {
        return Err(mlua::Error::external(IronCrewError::Validation(format!(
            "Crew.new {field} is {} bytes, exceeds {max_bytes}",
            value.len()
        ))));
    }
    Ok(())
}

fn validate_api_key_value(value: &str) -> LuaResult<()> {
    validate_config_string("api_key", value, MAX_API_KEY_BYTES)?;
    if value.trim() != value || value.chars().any(char::is_control) {
        return Err(mlua::Error::external(IronCrewError::Validation(
            "Crew.new api_key must not contain whitespace padding or control characters".into(),
        )));
    }
    Ok(())
}

fn trusted_provider_key_env_name(base_url: &str) -> Option<&'static str> {
    let parsed = reqwest::Url::parse(base_url).ok()?;
    if parsed.scheme() != "https" {
        return None;
    }
    match parsed.host_str()?.to_ascii_lowercase().as_str() {
        "api.openai.com" => Some("OPENAI_API_KEY"),
        "generativelanguage.googleapis.com" => Some("GEMINI_API_KEY"),
        "api.groq.com" => Some("GROQ_API_KEY"),
        "api.moonshot.ai" | "api.moonshot.cn" => Some("MOONSHOT_API_KEY"),
        "api.deepseek.com" => Some("DEEPSEEK_API_KEY"),
        "api.x.ai" => Some("XAI_API_KEY"),
        "api.openrouter.ai" => Some("OPENROUTER_API_KEY"),
        "api.anthropic.com" => Some("ANTHROPIC_API_KEY"),
        _ => None,
    }
}

fn resolve_custom_provider_key(
    base_url: Option<&str>,
    api_key: Option<&str>,
) -> LuaResult<Option<String>> {
    let resolved = api_key.map(str::to_owned).or_else(|| {
        base_url
            .and_then(trusted_provider_key_env_name)
            .and_then(|name| std::env::var(name).ok())
            .filter(|value| !value.trim().is_empty())
    });
    if base_url.is_some() && resolved.is_none() {
        return Err(mlua::Error::external(IronCrewError::Validation(
            "Crew.new with a non-canonical custom base_url requires an explicit api_key; process provider secrets are forwarded only to exact built-in HTTPS provider hosts".into(),
        )));
    }
    Ok(resolved)
}

fn strict_string_list(
    table: &Table,
    field: &str,
    max_items: usize,
    max_item_bytes: usize,
) -> LuaResult<Vec<String>> {
    let len = table.raw_len();
    if len > max_items {
        return Err(mlua::Error::external(IronCrewError::Validation(format!(
            "Crew.new {field} has {len} entries, exceeds {max_items}"
        ))));
    }

    // Reject map keys, sparse arrays, and trailing integer keys beyond
    // raw_len instead of silently ignoring them through sequence_values().
    let mut pair_count = 0usize;
    for pair in table.clone().pairs::<Value, Value>() {
        let (key, _) = pair?;
        pair_count = pair_count.saturating_add(1);
        match key {
            Value::Integer(index) if index >= 1 && (index as usize) <= len => {}
            _ => {
                return Err(mlua::Error::external(IronCrewError::Validation(format!(
                    "Crew.new {field} must be a dense array with integer keys starting at 1"
                ))));
            }
        }
    }
    if pair_count != len {
        return Err(mlua::Error::external(IronCrewError::Validation(format!(
            "Crew.new {field} must not contain gaps"
        ))));
    }

    let mut values = Vec::with_capacity(len);
    let mut seen = std::collections::HashSet::with_capacity(len);
    for index in 1..=len {
        let value = table.raw_get::<String>(index).map_err(|_| {
            mlua::Error::external(IronCrewError::Validation(format!(
                "Crew.new {field}[{index}] must be a string"
            )))
        })?;
        validate_config_string(&format!("{field}[{index}]"), &value, max_item_bytes)?;
        if !seen.insert(value.clone()) {
            return Err(mlua::Error::external(IronCrewError::Validation(format!(
                "Crew.new {field} contains duplicate entry '{value}'"
            ))));
        }
        values.push(value);
    }
    Ok(values)
}

// Preserve the established `crate::lua::api` import surface.
#[allow(unused_imports)]
pub use super::crew_userdata::LuaCrew;
#[allow(unused_imports)]
pub use super::json::{json_value_to_lua, lua_table_to_json, lua_value_to_json};
#[allow(unused_imports)]
pub use super::parsers::{
    LuaToolDef, agent_from_lua_table, load_agents_from_files, load_tool_defs_from_files,
    task_from_lua_table, tool_def_from_lua_table,
};

/// Marker type — when set as app-data on a Lua VM, signals that the VM is
/// being driven in chat REPL / HTTP conversation mode rather than the default
/// `run`-the-crew mode. Triggers:
///   1. Setting the `IRONCREW_MODE = "chat"` Lua global, and
///   2. Caching the most recently constructed `LuaCrew` userdata in the
///      registry under the `__ironcrew_chat_crew` slot so the chat harness
///      can retrieve it after the entrypoint script returns.
#[derive(Clone, Copy)]
pub struct ChatMode;

/// Registry key where `register_crew_constructor` stashes the most recently
/// constructed `LuaCrew` userdata when `ChatMode` app-data is set. Pulled by
/// the chat CLI / HTTP `start` handler to locate the crew that was just built
/// by the entrypoint script.
pub const CHAT_CREW_REGISTRY_KEY: &str = "__ironcrew_chat_crew";

/// Set the canonical `IRONCREW_MODE` Lua global. Users guard top-level
/// `crew:run()` with `if IRONCREW_MODE ~= "chat" then crew:run() end` so the
/// same `crew.lua` works for both `ironcrew run` and `ironcrew chat`.
pub fn set_ironcrew_mode(lua: &Lua, mode: &str) -> LuaResult<()> {
    lua.globals().set("IRONCREW_MODE", mode.to_string())
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

    // Defense-in-depth: seed sub-flow app-data on this VM too. The primary
    // seeding happens in `setup_crew_runtime`, but some callers (tests,
    // embeddings) build VMs directly and skip that path. Only `set` keys
    // that aren't already present so we don't stomp on a nested caller's
    // depth counter.
    if lua.app_data_ref::<Arc<Runtime>>().is_none() {
        lua.set_app_data(runtime.clone());
    }
    if lua.app_data_ref::<Arc<PathBuf>>().is_none() {
        lua.set_app_data(project_dir.clone());
    }
    if lua
        .app_data_ref::<crate::lua::subflow::SubflowDepth>()
        .is_none()
    {
        lua.set_app_data(crate::lua::subflow::SubflowDepth(0));
    }

    let new_fn = lua.create_async_function(move |lua, table: Table| {
        let agents = Arc::clone(&agents);
        let project_dir = (*project_dir).clone();
        let runtime = Arc::clone(&runtime);
        async move {

        // Shallow-merge defaults from config.lua (if present) into the user's
        // table. Only keys not already present are added — user values win.
        if let Ok(defaults) = lua.globals().get::<Table>("__ironcrew_config_defaults") {
            for pair in defaults.pairs::<mlua::Value, mlua::Value>() {
                let (key, value) = pair?;
                if let mlua::Value::String(ref s) = key
                    && !table.contains_key(s.clone())?
                {
                    table.set(key, value)?;
                }
            }
        }

        crate::lua::parsers::reject_unknown_keys(
            &table,
            crate::lua::parsers::CREW_KEYS,
            "Crew.new",
        )?;
        let goal: String = table.get("goal")?;
        let provider = table
            .get::<Option<String>>("provider")?
            .unwrap_or_else(|| "openai".into());
        let model = table
            .get::<Option<String>>("model")?
            .unwrap_or_else(|| crate::llm::DEFAULT_OPENAI_MODEL.into());
        let base_url: Option<String> = table.get("base_url")?;
        let api_key: Option<String> = table.get("api_key")?;
        let max_concurrent: Option<usize> = table.get("max_concurrent")?;
        let normalized_provider = provider.to_lowercase();

        validate_config_string("goal", &goal, MAX_GOAL_BYTES)?;
        validate_config_string("provider", &provider, MAX_PROVIDER_NAME_BYTES)?;
        validate_config_string("model", &model, MAX_MODEL_NAME_BYTES)?;
        if let Some(url) = base_url.as_deref() {
            crate::engine::conversation_provider::validate_provider_endpoint(url)
                .map_err(mlua::Error::external)?;
        }
        if let Some(key) = api_key.as_deref() {
            validate_api_key_value(key)?;
        }
        let custom_provider_key =
            resolve_custom_provider_key(base_url.as_deref(), api_key.as_deref())?;

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
                let key = custom_provider_key
                    .clone()
                    .or_else(|| {
                        base_url
                            .is_none()
                            .then(|| std::env::var("ANTHROPIC_API_KEY").ok())
                            .flatten()
                    })
                    .filter(|k| !k.trim().is_empty())
                    .ok_or_else(|| {
                        mlua::Error::external(IronCrewError::Validation(
                            "Anthropic provider requires an api_key or ANTHROPIC_API_KEY env var"
                                .to_string(),
                        ))
                    })?;
                validate_api_key_value(&key)?;

                // Parse Anthropic-specific config
                let thinking_budget: Option<u32> = table.get("thinking_budget")?;
                if thinking_budget.is_some_and(|value| value == 0 || value > MAX_THINKING_BUDGET)
                {
                    return Err(mlua::Error::external(IronCrewError::Validation(format!(
                        "Crew.new thinking_budget must be between 1 and {MAX_THINKING_BUDGET}"
                    ))));
                }

                let max_server_tools = config_limit(
                    "IRONCREW_MAX_SERVER_TOOLS",
                    DEFAULT_MAX_SERVER_TOOLS,
                    HARD_MAX_SERVER_TOOLS,
                )?;
                let server_tools_list = match table.get::<Option<Table>>("server_tools")? {
                    Some(tools) => strict_string_list(
                        &tools,
                        "server_tools",
                        max_server_tools,
                        MAX_CONFIG_ITEM_BYTES,
                    )?,
                    None => Vec::new(),
                };

                let web_search_max_uses: Option<u32> = table.get("web_search_max_uses")?;
                if web_search_max_uses
                    .is_some_and(|value| value == 0 || value > MAX_WEB_SEARCH_USES)
                {
                    return Err(mlua::Error::external(IronCrewError::Validation(format!(
                        "Crew.new web_search_max_uses must be between 1 and {MAX_WEB_SEARCH_USES}"
                    ))));
                }

                let server_tools: Vec<ServerTool> = server_tools_list
                    .iter()
                    .map(|name| match name.as_str() {
                        "web_search" => Ok(ServerTool::WebSearch {
                            max_uses: web_search_max_uses,
                        }),
                        "code_execution" => Ok(ServerTool::CodeExecution),
                        other => Err(mlua::Error::external(IronCrewError::Validation(format!(
                            "Unknown Anthropic server tool '{other}'"
                        )))),
                    })
                    .collect::<LuaResult<Vec<_>>>()?;

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
                let key = custom_provider_key
                    .clone()
                    .or_else(|| {
                        base_url
                            .is_none()
                            .then(|| std::env::var("OPENAI_API_KEY").ok())
                            .flatten()
                    })
                    .filter(|k| !k.trim().is_empty())
                    .ok_or_else(|| {
                        mlua::Error::external(IronCrewError::Validation(
                            "openai-responses provider requires an api_key or OPENAI_API_KEY env var"
                                .to_string(),
                        ))
                    })?;
                validate_api_key_value(&key)?;

                // Parse Responses-specific config
                let reasoning_effort: Option<String> = table.get("reasoning_effort")?;
                let reasoning_summary: Option<String> = table.get("reasoning_summary")?;
                if let Some(value) = reasoning_effort.as_deref() {
                    validate_config_string(
                        "reasoning_effort",
                        value,
                        MAX_CONFIG_ITEM_BYTES,
                    )?;
                }
                if let Some(value) = reasoning_summary.as_deref() {
                    validate_config_string(
                        "reasoning_summary",
                        value,
                        MAX_CONFIG_ITEM_BYTES,
                    )?;
                }

                let max_server_tools = config_limit(
                    "IRONCREW_MAX_SERVER_TOOLS",
                    DEFAULT_MAX_SERVER_TOOLS,
                    HARD_MAX_SERVER_TOOLS,
                )?;
                let server_tools_list = match table.get::<Option<Table>>("server_tools")? {
                    Some(tools) => strict_string_list(
                        &tools,
                        "server_tools",
                        max_server_tools,
                        MAX_CONFIG_ITEM_BYTES,
                    )?,
                    None => Vec::new(),
                };

                let max_vector_store_ids = config_limit(
                    "IRONCREW_MAX_VECTOR_STORE_IDS",
                    DEFAULT_MAX_VECTOR_STORE_IDS,
                    HARD_MAX_VECTOR_STORE_IDS,
                )?;
                let file_search_vector_store_ids =
                    match table.get::<Option<Table>>("file_search_vector_store_ids")? {
                        Some(ids) => strict_string_list(
                            &ids,
                            "file_search_vector_store_ids",
                            max_vector_store_ids,
                            MAX_CONFIG_ITEM_BYTES,
                        )?,
                        None => Vec::new(),
                    };

                let file_search_max_results: Option<u32> =
                    table.get("file_search_max_results")?;
                if file_search_max_results
                    .is_some_and(|value| value == 0 || value > MAX_FILE_SEARCH_RESULTS)
                {
                    return Err(mlua::Error::external(IronCrewError::Validation(format!(
                        "Crew.new file_search_max_results must be between 1 and {MAX_FILE_SEARCH_RESULTS}"
                    ))));
                }

                let web_search_context_size: Option<String> =
                    table.get("web_search_context_size")?;
                if let Some(value) = web_search_context_size.as_deref() {
                    validate_config_string(
                        "web_search_context_size",
                        value,
                        MAX_CONFIG_ITEM_BYTES,
                    )?;
                }

                let server_tools: Vec<ResponsesServerTool> = server_tools_list
                    .iter()
                    .map(|name| match name.as_str() {
                        "web_search" => Ok(ResponsesServerTool::WebSearch {
                            context_size: web_search_context_size.clone(),
                        }),
                        "file_search" => Ok(ResponsesServerTool::FileSearch {
                            vector_store_ids: file_search_vector_store_ids.clone(),
                            max_num_results: file_search_max_results,
                        }),
                        "code_interpreter" => Ok(ResponsesServerTool::CodeInterpreter),
                        other => Err(mlua::Error::external(IronCrewError::Validation(format!(
                            "Unknown Responses server tool '{other}'"
                        )))),
                    })
                    .collect::<LuaResult<Vec<_>>>()?;

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
                // A custom endpoint can receive only the key explicitly paired
                // with it in this Crew.new call. Never infer a process secret
                // from attacker-controlled hostname text.
                let key = custom_provider_key.clone().ok_or_else(|| {
                    mlua::Error::external(IronCrewError::Validation(
                        "Crew with custom provider settings requires an explicit api_key"
                            .to_string(),
                    ))
                })?;
                validate_api_key_value(&key)?;
                let url = base_url.clone();
                Some(Arc::new(OpenAiProvider::new(key, url)))
            } else {
                None
            };
        let custom_provider = custom_provider.map(crate::llm::metrics::observe_provider);

        let config = ProviderConfig {
            provider,
            model,
            base_url,
            api_key,
        };

        let memory_mode = table
            .get::<Option<String>>("memory")?
            .unwrap_or_else(|| "ephemeral".into());
        if !matches!(memory_mode.as_str(), "ephemeral" | "persistent") {
            return Err(mlua::Error::external(IronCrewError::Validation(format!(
                "Crew.new memory must be 'ephemeral' or 'persistent', got '{memory_mode}'"
            ))));
        }

        let defaults = MemoryConfig::default();
        let max_memory_items = config_limit(
            "IRONCREW_MAX_MEMORY_ITEMS",
            DEFAULT_MAX_MEMORY_ITEMS,
            HARD_MAX_MEMORY_ITEMS,
        )?;
        let max_memory_tokens = config_limit(
            "IRONCREW_MAX_MEMORY_TOKENS",
            DEFAULT_MAX_MEMORY_TOKENS,
            HARD_MAX_MEMORY_TOKENS,
        )?;
        let requested_memory_items: Option<usize> = table.get("max_memory_items")?;
        let requested_memory_tokens: Option<usize> = table.get("max_memory_tokens")?;
        if requested_memory_items.is_some_and(|value| value == 0 || value > max_memory_items) {
            return Err(mlua::Error::external(IronCrewError::Validation(format!(
                "Crew.new max_memory_items must be between 1 and IRONCREW_MAX_MEMORY_ITEMS ({max_memory_items})"
            ))));
        }
        if requested_memory_tokens.is_some_and(|value| value == 0 || value > max_memory_tokens) {
            return Err(mlua::Error::external(IronCrewError::Validation(format!(
                "Crew.new max_memory_tokens must be between 1 and IRONCREW_MAX_MEMORY_TOKENS ({max_memory_tokens})"
            ))));
        }
        let memory_config = MemoryConfig {
            max_items: requested_memory_items
                .or_else(|| defaults.max_items.map(|value| value.min(max_memory_items))),
            max_total_tokens: requested_memory_tokens.or_else(|| {
                defaults
                    .max_total_tokens
                    .map(|value| value.min(max_memory_tokens))
            }),
        };

        let memory = match memory_mode.as_str() {
            "persistent" => {
                let memory_path = project_dir.join(".ironcrew").join("memory.json");
                MemoryStore::persistent_with_config_async(memory_path, memory_config)
                    .await
                    .map_err(mlua::Error::external)?
            }
            "ephemeral" => MemoryStore::ephemeral_with_config(memory_config),
            _ => unreachable!("memory mode was validated above"),
        };

        let stream = table.get::<Option<bool>>("stream")?.unwrap_or(false);

        let model_router = if let Ok(models_table) = table.get::<mlua::Table>("models") {
            let max_routes = config_limit(
                "IRONCREW_MAX_MODEL_ROUTES",
                DEFAULT_MAX_MODEL_ROUTES,
                HARD_MAX_MODEL_ROUTES,
            )?;
            let mut router = ModelRouter::new();
            let mut count = 0usize;
            for pair in models_table.pairs::<String, String>() {
                let (purpose, model_name) = pair?;
                count = count.saturating_add(1);
                if count > max_routes {
                    return Err(mlua::Error::external(IronCrewError::Validation(format!(
                        "Crew.new models exceeds IRONCREW_MAX_MODEL_ROUTES ({max_routes})"
                    ))));
                }
                validate_config_string("models purpose", &purpose, MAX_CONFIG_ITEM_BYTES)?;
                validate_config_string("models value", &model_name, MAX_MODEL_NAME_BYTES)?;
                router.set(&purpose, model_name);
            }
            router
        } else if table.contains_key("models")? {
            return Err(mlua::Error::external(IronCrewError::Validation(
                "Crew.new models must be a table of string purpose-to-model entries".into(),
            )));
        } else {
            ModelRouter::new()
        };

        let prompt_cache_key: Option<String> = table.get("prompt_cache_key")?;
        let prompt_cache_retention: Option<String> = table.get("prompt_cache_retention")?;
        if let Some(value) = prompt_cache_key.as_deref() {
            validate_config_string("prompt_cache_key", value, MAX_CONFIG_ITEM_BYTES)?;
        }
        if let Some(value) = prompt_cache_retention.as_deref() {
            validate_config_string("prompt_cache_retention", value, MAX_CONFIG_ITEM_BYTES)?;
        }

        let mut crew = Crew::new(goal, config, memory);
        crew.max_concurrent_tasks = max_concurrent;
        crew.stream = stream;
        // Approval gates: tool names / prefix globs that need a human
        // sign-off before executing. Merged with IRONCREW_REQUIRE_APPROVAL
        // when the policy is attached at agent-tool finalization.
        if let Ok(Some(list)) = table.get::<Option<Table>>("require_approval") {
            let max_patterns = config_limit(
                "IRONCREW_MAX_APPROVAL_PATTERNS",
                128,
                1024,
            )?;
            crew.require_approval = strict_string_list(
                &list,
                "require_approval",
                max_patterns,
                512,
            )?;
        } else if table.contains_key("require_approval")? {
            return Err(mlua::Error::external(IronCrewError::Validation(
                "Crew.new require_approval must be an array of strings".into(),
            )));
        }
        crew.model_router = model_router;
        crew.prompt_cache_key = prompt_cache_key;
        crew.prompt_cache_retention = prompt_cache_retention;
        crew.validate_resource_limits()
            .map_err(mlua::Error::external)?;

        // Auto-inject preloaded agents from agents/ directory
        for agent in agents.iter() {
            crew.add_agent(agent.clone()).map_err(mlua::Error::external)?;
        }

        // ── MCP config (feature-gated) ──────────────────────────────────
        #[cfg(feature = "mcp")]
        let mcp_config = match table.get::<Option<Table>>("mcp_servers")? {
            Some(mcp_table) => Some(parse_mcp_config(&mcp_table)?),
            None => None,
        };

        // If the host (e.g. the HTTP server) has provided a shared store
        // via app_data, prefill the LuaCrew's `OnceCell` with it so every
        // Lua-triggered store access reuses the server-wide `Arc` — no
        // re-bootstrap and no extra Postgres pool per request.
        let store_cell = tokio::sync::OnceCell::new();
        if let Some(shared) = lua.app_data_ref::<Arc<dyn StateStore>>() {
            let _ = store_cell.set(shared.clone());
        }

        let lua_crew = LuaCrew {
            crew: Arc::new(Mutex::new(crew)),
            runtime: runtime.clone(),
            custom_provider,
            project_dir,
            store: store_cell,
            #[cfg(feature = "mcp")]
            mcp_config,
            #[cfg(feature = "mcp")]
            mcp_manager: Arc::new(tokio::sync::Mutex::new(None)),
            #[cfg(feature = "mcp")]
            mcp_tool_registry: Arc::new(tokio::sync::Mutex::new(None)),
            agent_tools_finalized: tokio::sync::OnceCell::new(),
        };

        // In chat mode, stash the userdata in the registry so the CLI/HTTP
        // harness can pick it back up once the entrypoint script returns.
        // We do this by constructing an AnyUserData and retrieving it via
        // create_userdata, then storing it under a named registry slot.
        if lua.app_data_ref::<ChatMode>().is_some() {
            let ud = lua.create_userdata(lua_crew)?;
            lua.set_named_registry_value(CHAT_CREW_REGISTRY_KEY, ud.clone())?;
            return Ok(mlua::Value::UserData(ud));
        }

        let ud = lua.create_userdata(lua_crew)?;
        Ok(mlua::Value::UserData(ud))
        }
    })?;

    crew_table.set("new", new_fn)?;
    lua.globals().set("Crew", crew_table)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        resolve_custom_provider_key, strict_string_list, trusted_provider_key_env_name,
        validate_api_key_value, validate_config_string,
    };
    use mlua::Lua;

    #[test]
    fn strict_string_list_rejects_sparse_mixed_and_duplicate_values() {
        let lua = Lua::new();

        let sparse = lua.create_table().unwrap();
        sparse.raw_set(1, "one").unwrap();
        sparse.raw_set(3, "three").unwrap();
        assert!(strict_string_list(&sparse, "items", 8, 32).is_err());

        let mixed = lua.create_table().unwrap();
        mixed.raw_set(1, "one").unwrap();
        mixed.set("extra", "two").unwrap();
        assert!(strict_string_list(&mixed, "items", 8, 32).is_err());

        let duplicate = lua.create_sequence_from(["one", "one"]).unwrap();
        assert!(strict_string_list(&duplicate, "items", 8, 32).is_err());
    }

    #[test]
    fn strict_string_list_preserves_valid_dense_order() {
        let lua = Lua::new();
        let table = lua.create_sequence_from(["one", "two"]).unwrap();
        assert_eq!(
            strict_string_list(&table, "items", 2, 8).unwrap(),
            vec!["one", "two"]
        );
    }

    #[test]
    fn config_strings_and_api_keys_are_bounded() {
        assert!(validate_config_string("goal", "", 10).is_err());
        assert!(validate_config_string("goal", "eleven bytes", 10).is_err());
        assert!(validate_config_string("goal", "valid", 10).is_ok());
        assert!(validate_api_key_value(" padded").is_err());
        assert!(validate_api_key_value("valid-key").is_ok());
    }

    #[test]
    fn custom_provider_url_never_inherits_a_process_secret_for_untrusted_hosts() {
        assert!(resolve_custom_provider_key(Some("https://attacker.example/v1"), None).is_err());
        assert!(
            resolve_custom_provider_key(
                Some("https://attacker.example/v1"),
                Some("caller-owned-key")
            )
            .is_ok()
        );
        assert!(resolve_custom_provider_key(None, None).is_ok());
        assert_eq!(
            trusted_provider_key_env_name(
                "https://generativelanguage.googleapis.com/v1beta/openai"
            ),
            Some("GEMINI_API_KEY")
        );
        assert_eq!(
            trusted_provider_key_env_name("https://api.openai.com.attacker.example/v1"),
            None
        );
        assert_eq!(
            trusted_provider_key_env_name("http://api.openai.com/v1"),
            None
        );
    }
}
