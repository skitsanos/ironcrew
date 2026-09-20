use super::*;

/// Build an AgentDialog from a Lua options table. Participants are given via
/// the `agents = {"name", ...}` array (two or more).
///
/// When `store` and the caller-provided `id` are both present, the dialog is
/// resumed from the store if a prior record exists (transcript, `next_index`,
/// and stop state). Autosave defaults to `true` for persistent sessions.
#[allow(clippy::too_many_arguments)]
pub async fn build_dialog(
    lua: &mlua::Lua,
    table: Table,
    crew_agents: &[Agent],
    provider: Arc<dyn LlmProvider>,
    tool_registry: ToolRegistry,
    crew_default_model: &str,
    crew_max_tool_rounds: usize,
    eventbus: EventBus,
    store: Option<Arc<dyn StateStore>>,
    flow_name: String,
    flow_path: Option<String>,
) -> mlua::Result<AgentDialog> {
    crate::lua::parsers::option_keys::reject_dialog_keys(&table)?;
    let agents_table = table.get::<Table>("agents").map_err(|_| {
        mlua::Error::external(IronCrewError::Validation(
            "Dialog requires an `agents = {\"name\", ...}` array of two or more \
             participants"
                .into(),
        ))
    })?;
    let mut dialog_agents: Vec<Agent> = Vec::new();
    for value in agents_table.sequence_values::<Value>() {
        let value = value?;
        dialog_agents.push(resolve_agent(value, crew_agents, "agents")?);
    }

    if dialog_agents.len() < 2 {
        return Err(mlua::Error::external(IronCrewError::Validation(
            "Dialog requires at least 2 agents".into(),
        )));
    }
    let participant_limit = configured_dialog_max_participants();
    if dialog_agents.len() > participant_limit {
        return Err(mlua::Error::external(IronCrewError::Validation(format!(
            "Dialog supports at most {participant_limit} participants, got {}",
            dialog_agents.len()
        ))));
    }

    // Reject duplicate names — each agent must be distinct
    {
        let mut seen = std::collections::HashSet::new();
        for a in &dialog_agents {
            if !seen.insert(a.name.as_str()) {
                return Err(mlua::Error::external(IronCrewError::Validation(format!(
                    "Dialog: agent '{}' is listed more than once",
                    a.name
                ))));
            }
        }
    }

    let starter: String = table.get("starter").map_err(|_| {
        mlua::Error::external(IronCrewError::Validation(
            "Dialog requires a 'starter' string".into(),
        ))
    })?;

    let max_turns: usize = table
        .get::<usize>("max_turns")
        .unwrap_or(dialog_agents.len() * 2);
    let turn_limit = configured_dialog_max_turns();
    if max_turns == 0 || max_turns > turn_limit {
        return Err(mlua::Error::external(IronCrewError::Validation(format!(
            "Dialog max_turns must be between 1 and {turn_limit}, got {max_turns}"
        ))));
    }
    // max_history resolution order (same pattern as LuaConversation):
    //   1. Explicit positive value in the Lua table
    //   2. IRONCREW_DIALOG_MAX_HISTORY env var
    //   3. Safe default of 100 turns
    let max_history: Option<usize> = match table.get::<usize>("max_history") {
        Ok(n) if (1..=HARD_DIALOG_MAX_HISTORY).contains(&n) => Some(n),
        Ok(n) => {
            return Err(mlua::Error::external(IronCrewError::Validation(format!(
                "Dialog max_history must be between 1 and {HARD_DIALOG_MAX_HISTORY}, got {n}"
            ))));
        }
        Err(_) => Some(default_dialog_max_history()),
    };
    let stream: bool = table.get::<bool>("stream").unwrap_or(false);

    // starting_speaker accepts:
    //   - an agent name (preferred for multi-party)
    //   - a positional letter "a", "b", "c", ...
    //   - default: first agent (index 0)
    let starting_speaker: usize = match table.get::<String>("starting_speaker").ok() {
        Some(s) => {
            // Try as agent name first
            if let Some(idx) = dialog_agents.iter().position(|a| a.name == s) {
                idx
            } else if s.len() == 1 {
                let c = s.chars().next().unwrap().to_ascii_lowercase();
                if c.is_ascii_alphabetic() {
                    let idx = (c as u8 - b'a') as usize;
                    if idx < dialog_agents.len() {
                        idx
                    } else {
                        return Err(mlua::Error::external(IronCrewError::Validation(format!(
                            "Dialog: starting_speaker '{}' is out of range (only {} agents)",
                            s,
                            dialog_agents.len()
                        ))));
                    }
                } else {
                    0
                }
            } else {
                return Err(mlua::Error::external(IronCrewError::Validation(format!(
                    "Dialog: starting_speaker '{}' does not match any agent in this dialog",
                    s
                ))));
            }
        }
        None => 0,
    };

    let model: String = table
        .get::<String>("model")
        .ok()
        .unwrap_or_else(|| crew_default_model.to_string());

    // Optional turn_selector callback — stored in the Lua registry for thread safety
    let turn_selector_key: Option<mlua::RegistryKey> =
        if let Ok(func) = table.get::<mlua::Function>("turn_selector") {
            Some(lua.create_registry_value(func)?)
        } else {
            None
        };

    // Optional should_stop callback — same registry-key pattern as turn_selector
    let should_stop_key: Option<mlua::RegistryKey> =
        if let Ok(func) = table.get::<mlua::Function>("should_stop") {
            Some(lua.create_registry_value(func)?)
        } else {
            None
        };

    // Cross-run persistence: `id` is the persistence key. When omitted the
    // dialog is ephemeral (pre-2.8 behavior — a fresh UUID is generated).
    let id: Option<String> = table.get::<String>("id").ok();
    let autosave: bool = table.get::<bool>("autosave").unwrap_or(true);

    AgentDialog::new_or_resume(
        dialog_agents,
        provider,
        tool_registry,
        model,
        starter,
        max_turns,
        max_history,
        stream,
        crew_max_tool_rounds,
        starting_speaker,
        eventbus,
        turn_selector_key,
        should_stop_key,
        id,
        store,
        flow_name,
        flow_path,
        autosave,
    )
    .await
    .map_err(mlua::Error::external)
}

fn resolve_agent(value: Value, agents: &[Agent], field: &str) -> mlua::Result<Agent> {
    match value {
        Value::String(s) => {
            let name = s.to_str()?.to_string();
            agents
                .iter()
                .find(|a| a.name == name)
                .cloned()
                .ok_or_else(|| {
                    mlua::Error::external(IronCrewError::Validation(format!(
                        "Dialog: {} agent '{}' not found in crew",
                        field, name
                    )))
                })
        }
        Value::Table(t) => crate::lua::parsers::agent_from_lua_table(&t),
        _ => Err(mlua::Error::external(IronCrewError::Validation(format!(
            "Dialog: {} must be a string (agent name) or Agent table",
            field
        )))),
    }
}
