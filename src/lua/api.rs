use std::path::PathBuf;
use std::sync::Arc;

use mlua::{Lua, Result as LuaResult, Table};
use tokio::sync::Mutex;

use crate::engine::agent::Agent;
use crate::engine::crew::{Crew, ProviderConfig};
use crate::engine::memory::{MemoryConfig, MemoryStore};
use crate::engine::runtime::Runtime;
use crate::llm::openai::OpenAiProvider;
use crate::llm::provider::LlmProvider;

// Re-export everything that was previously defined here so that existing
// import paths (`crate::lua::api::…`) continue to work unchanged.
// Some re-exports are only consumed by integration tests or downstream crates.
#[allow(unused_imports)]
pub use super::crew_userdata::LuaCrew;
#[allow(unused_imports)]
pub use super::json::{json_value_to_lua, lua_table_to_json, lua_value_to_json};
#[allow(unused_imports)]
pub use super::parsers::{
    agent_from_lua_table, load_agents_from_files, load_tool_defs_from_files, task_from_lua_table,
    tool_def_from_lua_table, LuaToolDef,
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
        let provider: String =
            table.get::<String>("provider").unwrap_or_else(|_| "openai".into());
        let model: String =
            table.get::<String>("model").unwrap_or_else(|_| "gpt-4o-mini".into());
        let base_url: Option<String> = table.get("base_url").ok();
        let api_key: Option<String> = table.get("api_key").ok();
        let max_concurrent: Option<usize> = table.get::<Option<usize>>("max_concurrent").ok().flatten();

        // Create a custom provider if api_key or base_url differ from defaults
        let custom_provider: Option<Arc<dyn LlmProvider>> =
            if api_key.is_some() || base_url.is_some() {
                let key = api_key
                    .clone()
                    .or_else(|| std::env::var("OPENAI_API_KEY").ok())
                    .unwrap_or_default();
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

        let max_memory_items: Option<usize> = table.get("max_memory_items").ok();
        let max_memory_tokens: Option<usize> = table.get("max_memory_tokens").ok();

        let memory_config = MemoryConfig {
            max_items: max_memory_items,
            max_total_tokens: max_memory_tokens,
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

        let mut crew = Crew::new(goal, config, memory);
        crew.max_concurrent_tasks = max_concurrent;
        crew.stream = stream;

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
