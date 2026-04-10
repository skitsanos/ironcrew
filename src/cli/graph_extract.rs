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

    // Register stub globals for APIs that crew.lua scripts may use before
    // reaching Crew.new(). Without these, scripts like stock-debate crash
    // on `http.get()` or `json_parse()` before any crew data is captured.
    register_stub_globals(&lua).map_err(IronCrewError::Lua)?;

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

            // conversation() returns a stub with send/ask/history/reset/length methods.
            let conv_fn = lua.create_function(|lua, _args: mlua::MultiValue| {
                let t = lua.create_table()?;
                t.set("send", lua.create_function(|_, _args: mlua::MultiValue| Ok(""))?)?;
                t.set("ask", lua.create_function(|lua, _args: mlua::MultiValue| {
                    let r = lua.create_table()?;
                    r.set("content", "")?;
                    Ok(r)
                })?)?;
                t.set("history", lua.create_function(|lua, _args: mlua::MultiValue| lua.create_table())?)?;
                t.set("reset", lua.create_function(|_, _args: mlua::MultiValue| Ok(()))?)?;
                t.set("length", lua.create_function(|_, _args: mlua::MultiValue| Ok(0))?)?;
                t.set("agent_name", lua.create_function(|_, _args: mlua::MultiValue| Ok(""))?)?;
                t.set("id", lua.create_function(|_, _args: mlua::MultiValue| Ok(""))?)?;
                Ok(mlua::Value::Table(t))
            })?;
            crew_proxy.set("conversation", conv_fn)?;

            // dialog() returns a stub with run/next_turn/transcript/agents etc.
            let dialog_fn = lua.create_function(|lua, _args: mlua::MultiValue| {
                let t = lua.create_table()?;
                t.set("run", lua.create_function(|lua, _args: mlua::MultiValue| lua.create_table())?)?;
                t.set("next_turn", lua.create_function(|_, _args: mlua::MultiValue| Ok(mlua::Value::Nil))?)?;
                t.set("next_turn_from", lua.create_function(|_, _args: mlua::MultiValue| Ok(mlua::Value::Nil))?)?;
                t.set("transcript", lua.create_function(|lua, _args: mlua::MultiValue| lua.create_table())?)?;
                t.set("turn_count", lua.create_function(|_, _args: mlua::MultiValue| Ok(0))?)?;
                t.set("current_speaker", lua.create_function(|_, _args: mlua::MultiValue| Ok(mlua::Value::Nil))?)?;
                t.set("current_agent", lua.create_function(|_, _args: mlua::MultiValue| Ok(mlua::Value::Nil))?)?;
                t.set("agents", lua.create_function(|lua, _args: mlua::MultiValue| lua.create_table())?)?;
                t.set("reset", lua.create_function(|_, _args: mlua::MultiValue| Ok(()))?)?;
                t.set("max_turns", lua.create_function(|_, _args: mlua::MultiValue| Ok(0))?)?;
                t.set("stopped", lua.create_function(|_, _args: mlua::MultiValue| Ok(false))?)?;
                t.set("stop_reason", lua.create_function(|_, _args: mlua::MultiValue| Ok(mlua::Value::Nil))?)?;
                t.set("id", lua.create_function(|_, _args: mlua::MultiValue| Ok(""))?)?;
                Ok(mlua::Value::Table(t))
            })?;
            crew_proxy.set("dialog", dialog_fn)?;

            // memory() returns nil (key-value lookup returns nothing).
            let mem_fn = lua.create_function(|_, _args: mlua::MultiValue| {
                Ok(mlua::Value::Nil)
            })?;
            crew_proxy.set("memory", mem_fn)?;

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

    // First pass: try executing the full script. Errors are caught by
    // wrapping in pcall so the Lua VM stays alive.
    let wrapped = format!(
        "local __ok, __err = pcall(function()\n{}\nend)\n",
        source
    );
    match lua.load(&wrapped).exec() {
        Ok(_) => {}
        Err(e) => {
            tracing::debug!("graph_extract: crew.lua execution stopped: {}", e);
        }
    }

    // If the first pass captured nothing (common when the script depends
    // on live data that stubs can't satisfy), try a fallback: extract just
    // the crew-setup portion starting from `Crew.new(` and execute that.
    // This skips the data-processing preamble that crashes on nil values.
    {
        let captured_so_far = capture.lock().unwrap();
        if captured_so_far.goal.is_empty()
            && captured_so_far.tasks.is_empty()
            && captured_so_far.inline_agents.is_empty()
        {
            drop(captured_so_far);
            if let Some(crew_start) = source.find("Crew.new(") {
                tracing::debug!(
                    "graph_extract: first pass captured nothing, trying crew-setup fallback"
                );
                // Extract local constant definitions from the preamble
                // (before Crew.new) so variables like TICKER, RANGE are
                // available when the crew-setup portion executes.
                let preamble = &source[..crew_start];
                let locals = extract_local_constants(preamble);

                let crew_portion = &source[crew_start..];
                let fallback = format!(
                    "{}\nlocal __ok, __err = pcall(function()\nlocal crew = {}\nend)\n",
                    locals, crew_portion
                );
                match lua.load(&fallback).exec() {
                    Ok(_) => {}
                    Err(e) => {
                        tracing::debug!("graph_extract: fallback also stopped: {}", e);
                    }
                }
            }
        }
    }

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

/// Register stub globals for common APIs that crew.lua scripts may call
/// before (or instead of) `Crew.new()`. Without these, scripts that use
/// `http.get()`, `json_parse()`, etc. crash before any crew data is
/// captured — making the graph command useless for those projects.
fn register_stub_globals(lua: &mlua::Lua) -> mlua::Result<()> {
    // A "deep nil" proxy: any property chain like `t.a.b.c[1].d` returns
    // another proxy instead of crashing. This lets scripts that process
    // HTTP/JSON data before reaching Crew.new() get past the data section.
    lua.load(
        r#"
        function __ironcrew_deep_nil()
            local proxy = {}
            setmetatable(proxy, {
                __index    = function() return __ironcrew_deep_nil() end,
                __call     = function() return __ironcrew_deep_nil() end,
                __len      = function() return 0 end,
                __tostring = function() return "" end,
                __concat   = function(a, b) return tostring(a) .. tostring(b) end,
                __add      = function() return 0 end,
                __sub      = function() return 0 end,
                __mul      = function() return 0 end,
                __div      = function() return 0 end,
                __mod      = function() return 0 end,
                __unm      = function() return 0 end,
                __lt       = function() return false end,
                __le       = function() return false end,
                __eq       = function() return false end,
            })
            return proxy
        end
    "#,
    )
    .exec()?;

    // http.get / http.post / http.request — return a response whose `.json`
    // is a deep-nil proxy so property chains don't crash.
    let http = lua.create_table()?;
    let http_stub = lua.create_function(|lua, _args: mlua::MultiValue| {
        let resp = lua.create_table()?;
        resp.set("ok", true)?;
        resp.set("status", 200)?;
        resp.set("body", "")?;
        let deep_nil: mlua::Function = lua.globals().get("__ironcrew_deep_nil")?;
        resp.set("json", deep_nil.call::<mlua::Value>(())?)?;
        Ok(resp)
    })?;
    http.set("get", http_stub.clone())?;
    http.set("post", http_stub.clone())?;
    http.set("put", http_stub.clone())?;
    http.set("delete", http_stub.clone())?;
    http.set("request", http_stub)?;
    lua.globals().set("http", http)?;

    // json_parse — returns an empty table for any input
    lua.globals().set(
        "json_parse",
        lua.create_function(|lua, _input: mlua::Value| lua.create_table())?,
    )?;

    // json_stringify — returns "{}" for any input
    lua.globals().set(
        "json_stringify",
        lua.create_function(|_, _input: mlua::Value| Ok("{}".to_string()))?,
    )?;

    // print — no-op in capture mode (suppress all output)
    lua.globals().set(
        "print",
        lua.create_function(|_, _args: mlua::MultiValue| Ok(()))?,
    )?;

    // error — no-op in capture mode. Scripts that validate runtime data
    // (e.g., "if #series < 50 then error(...)") would abort before
    // reaching Crew.new(). Swallowing error() lets the script continue
    // past validation checks into the crew setup code.
    lua.globals().set(
        "error",
        lua.create_function(|_, _args: mlua::MultiValue| Ok(()))?,
    )?;

    Ok(())
}

/// Extract simple `local X = "string"` and `local X = number` constant
/// definitions from Lua source. These are safe to prepend to a partial
/// script execution so that variables referenced after Crew.new() are
/// available even when we skip the data-processing preamble.
fn extract_local_constants(source: &str) -> String {
    let mut constants = String::new();
    for line in source.lines() {
        let trimmed = line.trim();
        // Match: local IDENT = "string" | local IDENT = number | local IDENT = true/false
        if trimmed.starts_with("local ") && trimmed.contains('=') {
            let after_eq = trimmed.splitn(2, '=').nth(1).unwrap_or("").trim();
            let is_simple = after_eq.starts_with('"')
                || after_eq.starts_with('\'')
                || after_eq.starts_with("[[")
                || after_eq == "true"
                || after_eq == "false"
                || after_eq == "nil"
                || after_eq.parse::<f64>().is_ok();
            if is_simple {
                constants.push_str(trimmed);
                constants.push('\n');
            }
        }
    }
    constants
}
