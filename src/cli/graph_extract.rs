use std::path::Path;
use std::sync::{Arc, Mutex};

use mlua::Table;

use crate::cli::graph_types::*;
use crate::cli::project::load_project;
use crate::engine::agent::Agent;
use crate::engine::task::Task;
use crate::lua::api::{
    agent_from_lua_table, load_agents_from_files, load_tool_defs_from_files,
    register_agent_constructor, task_from_lua_table,
};
use crate::lua::sandbox::create_crew_lua;
use crate::utils::error::{IronCrewError, Result};

// ---------------------------------------------------------------------------
// Internal capture accumulator
// ---------------------------------------------------------------------------

#[derive(Default)]
struct CapturedCrew {
    goal: String,
    provider: String,
    model: String,
    inline_agents: Vec<Agent>,
    tasks: Vec<Task>,
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Parse a crew project at `path` and extract graph data without executing any
/// LLM calls. `crew.lua` is run in a capture-mode sandbox where `Crew.new()`,
/// `crew:add_task()`, `crew:add_agent()` and `crew:run()` are all stubbed to
/// only record their arguments.
pub fn extract_graph_data(path: &Path) -> Result<GraphData> {
    // ------------------------------------------------------------------
    // 1. Discover project files (agent/*.lua, tools/*.lua, crew.lua)
    // ------------------------------------------------------------------
    let loader = load_project(path)?;

    // ------------------------------------------------------------------
    // 2. Load declarative agents and tool defs from files
    // ------------------------------------------------------------------
    let file_agents: Vec<Agent> = load_agents_from_files(loader.agent_files())?;
    let tool_defs = load_tool_defs_from_files(loader.tool_files())?;

    // ------------------------------------------------------------------
    // 3. Build a capture-mode Lua VM
    // ------------------------------------------------------------------
    let lua = create_crew_lua().map_err(IronCrewError::Lua)?;

    // Register Agent.new() so inline Agent definitions parse correctly.
    register_agent_constructor(&lua).map_err(IronCrewError::Lua)?;

    // Override the env() global with a simple, unrestricted version so that
    // crew.lua env() calls return None instead of erroring or blocking.
    let env_fn = lua
        .create_function(|_, key: String| Ok(std::env::var(&key).ok()))
        .map_err(IronCrewError::Lua)?;
    lua.globals()
        .set("env", env_fn)
        .map_err(IronCrewError::Lua)?;

    // ------------------------------------------------------------------
    // 4. Shared capture state
    // ------------------------------------------------------------------
    let capture: Arc<Mutex<CapturedCrew>> = Arc::new(Mutex::new(CapturedCrew::default()));

    // ------------------------------------------------------------------
    // 5. Register capture-mode Crew.new()
    // ------------------------------------------------------------------
    let capture_for_crew = capture.clone();

    let crew_table = lua.create_table().map_err(IronCrewError::Lua)?;

    let new_fn = lua
        .create_function(move |lua, table: Table| {
            // Extract crew-level metadata
            let goal: String = table.get("goal").unwrap_or_default();
            let provider: String = table
                .get::<String>("provider")
                .unwrap_or_else(|_| "openai".into());
            let model: String = table
                .get::<String>("model")
                .unwrap_or_else(|_| "gpt-4.1-mini".into());

            {
                let mut cap = capture_for_crew.lock().unwrap();
                cap.goal = goal;
                cap.provider = provider;
                cap.model = model;
            }

            // Build a crew proxy table that captures add_agent / add_task calls.
            let crew_proxy = lua.create_table()?;

            let cap_add_agent = capture_for_crew.clone();
            let add_agent_fn =
                lua.create_function(move |_, (_self, agent_table): (mlua::Value, Table)| {
                    match agent_from_lua_table(&agent_table) {
                        Ok(agent) => {
                            cap_add_agent.lock().unwrap().inline_agents.push(agent);
                        }
                        Err(e) => {
                            tracing::warn!("graph_extract: failed to parse inline agent: {}", e);
                        }
                    }
                    Ok(())
                })?;
            crew_proxy.set("add_agent", add_agent_fn)?;

            let cap_add_task = capture_for_crew.clone();
            let add_task_fn =
                lua.create_function(move |_, (_self, task_table): (mlua::Value, Table)| {
                    match task_from_lua_table(&task_table) {
                        Ok(task) => {
                            cap_add_task.lock().unwrap().tasks.push(task);
                        }
                        Err(e) => {
                            tracing::warn!("graph_extract: failed to parse task: {}", e);
                        }
                    }
                    Ok(())
                })?;
            crew_proxy.set("add_task", add_task_fn)?;

            // run() returns an empty table — no LLM calls.
            let run_fn = lua.create_function(|lua, _self: mlua::Value| {
                let t = lua.create_table()?;
                Ok(t)
            })?;
            crew_proxy.set("run", run_fn)?;

            // conversation() / dialog() / memory() return stubs.
            let stub_fn = lua.create_function(|lua, _args: mlua::MultiValue| {
                let t = lua.create_table()?;
                Ok(mlua::Value::Table(t))
            })?;
            crew_proxy.set("conversation", stub_fn.clone())?;
            crew_proxy.set("dialog", stub_fn.clone())?;
            crew_proxy.set("memory", stub_fn)?;

            // Set __index so method calls work on the proxy table.
            let mt = lua.create_table()?;
            mt.set("__index", crew_proxy.clone())?;
            let _ = crew_proxy.set_metatable(Some(mt));

            Ok(crew_proxy)
        })
        .map_err(IronCrewError::Lua)?;

    crew_table.set("new", new_fn).map_err(IronCrewError::Lua)?;
    lua.globals()
        .set("Crew", crew_table)
        .map_err(IronCrewError::Lua)?;

    // ------------------------------------------------------------------
    // 6. Execute crew.lua — ignore errors (script may fail after run())
    // ------------------------------------------------------------------
    let entrypoint = loader
        .entrypoint()
        .ok_or_else(|| IronCrewError::Validation("No entrypoint found".into()))?;

    let source = std::fs::read_to_string(entrypoint).map_err(IronCrewError::Io)?;
    let _ = lua.load(&source).exec();

    // ------------------------------------------------------------------
    // 7. Assemble GraphData from captured + file-based data
    // ------------------------------------------------------------------
    let captured = capture.lock().unwrap();

    // Merge agents: file-based agents (auto_discovered) first, then any
    // inline-only agents that are not already present by name.
    let file_agent_names: std::collections::HashSet<String> =
        file_agents.iter().map(|a| a.name.clone()).collect();
    let mut all_agents: Vec<Agent> = file_agents;
    for inline in &captured.inline_agents {
        if !all_agents.iter().any(|a| a.name == inline.name) {
            all_agents.push(inline.clone());
        }
    }

    // Build tool-ownership map: tool_name -> agent_name
    let mut tool_owner: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    for agent in &all_agents {
        for tool in &agent.tools {
            tool_owner
                .entry(tool.clone())
                .or_insert_with(|| agent.name.clone());
        }
    }

    // GraphAgents
    let graph_agents: Vec<GraphAgent> = all_agents
        .iter()
        .map(|a| {
            let source = if file_agent_names.contains(&a.name) {
                "auto_discovered"
            } else {
                "inline"
            };
            GraphAgent {
                name: a.name.clone(),
                goal: a.goal.clone(),
                capabilities: a.capabilities.clone(),
                tools: a.tools.clone(),
                source: source.into(),
                temperature: a.temperature,
            }
        })
        .collect();

    // GraphTools
    let graph_tools: Vec<GraphTool> = tool_defs
        .iter()
        .map(|t| GraphTool {
            name: t.name.clone(),
            description: t.description.clone(),
            owner: tool_owner.get(&t.name).cloned(),
            source: t
                .source_path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_string(),
        })
        .collect();

    // GraphTasks
    let graph_tasks: Vec<GraphTask> = captured
        .tasks
        .iter()
        .map(|t| {
            let task_type = derive_task_type(t);
            let assignment_source = if t.agent.is_some() {
                "explicit".to_string()
            } else {
                "auto".to_string()
            };

            let resolved_agent = if t.agent.is_none() && !all_agents.is_empty() {
                Some(auto_resolve_agent(t, &all_agents))
            } else {
                None
            };

            let display_name = pretty_name(&t.name);

            GraphTask {
                id: t.name.clone(),
                name: display_name,
                task_type,
                description: t.description.clone(),
                depends_on: t.depends_on.clone(),
                agent: t.agent.clone(),
                resolved_agent,
                assignment_source,
                expected_output: t.expected_output.clone(),
            }
        })
        .collect();

    Ok(GraphData {
        name: entrypoint
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .unwrap_or("crew")
            .to_string(),
        provider: captured.provider.clone(),
        model: captured.model.clone(),
        goal: captured.goal.clone(),
        agents: graph_agents,
        tools: graph_tools,
        tasks: graph_tasks,
        ..Default::default()
    })
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Derive a display task_type string from Task flags.
fn derive_task_type(task: &Task) -> String {
    if task.foreach_source.is_some() && task.foreach_parallel {
        return "foreach_parallel".into();
    }
    if task.foreach_source.is_some() {
        return "foreach".into();
    }
    if task.condition.is_some() {
        return "condition".into();
    }
    if task.max_retries.map(|r| r > 0).unwrap_or(false) {
        return "retry".into();
    }
    if task
        .task_type
        .as_deref()
        .map(|tt| tt == "collaborative")
        .unwrap_or(false)
    {
        return "collaborative".into();
    }
    "task".into()
}

/// Simple capability-keyword match to propose an agent for auto-assigned tasks.
/// Returns the name of the highest-scoring agent, falling back to the first agent.
fn auto_resolve_agent(task: &Task, agents: &[Agent]) -> String {
    let description_lower = task.description.to_lowercase();

    let best = agents
        .iter()
        .enumerate()
        .max_by(|(idx_a, a), (idx_b, b)| {
            let score_a = capability_score(a, &description_lower);
            let score_b = capability_score(b, &description_lower);
            score_a
                .partial_cmp(&score_b)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(idx_b.cmp(idx_a)) // tie: prefer earlier agent
        })
        .map(|(_, agent)| agent);

    best.map(|a| a.name.clone())
        .unwrap_or_else(|| agents[0].name.clone())
}

/// Count how many of an agent's capabilities appear as words in the description.
fn capability_score(agent: &Agent, description_lower: &str) -> usize {
    agent
        .capabilities
        .iter()
        .filter(|cap| description_lower.contains(cap.to_lowercase().as_str()))
        .count()
}

/// Convert a snake_case task name to a human-readable display name.
/// "fetch_data" -> "Fetch data"
fn pretty_name(name: &str) -> String {
    let replaced = name.replace('_', " ");
    let mut chars = replaced.chars();
    match chars.next() {
        None => String::new(),
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
    }
}
