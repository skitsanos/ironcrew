use super::*;

/// Build a LuaConversation from a Lua options table, agent lookup, provider,
/// tool registry, and crew defaults.
///
/// When `store` and the caller-provided `id` are both present, the conversation
/// is resumed from the store if a prior record exists. Autosave defaults to
/// `true` for persistent sessions and is a no-op for ephemeral ones.
#[allow(clippy::too_many_arguments)]
pub async fn build_conversation(
    lua: &Lua,
    table: Table,
    agents: &[Agent],
    provider: Arc<dyn LlmProvider>,
    tool_registry: ToolRegistry,
    crew_default_model: &str,
    source_fingerprint: &str,
    crew_max_tool_rounds: usize,
    eventbus: EventBus,
    store: Option<Arc<dyn StateStore>>,
    flow_name: String,
    flow_path: Option<String>,
    project_dir: std::path::PathBuf,
    http_client: reqwest::Client,
) -> mlua::Result<LuaConversation> {
    crate::lua::parsers::option_keys::reject_conversation_keys(&table)?;
    // Resolve agent: either by name or inline (Agent table)
    let agent_value: Value = table.get("agent")?;
    let agent: Agent = match agent_value {
        Value::String(s) => {
            let name = s.to_str()?.to_string();
            agents
                .iter()
                .find(|a| a.name == name)
                .cloned()
                .ok_or_else(|| {
                    mlua::Error::external(IronCrewError::Validation(format!(
                        "Conversation: agent '{}' not found in crew",
                        name
                    )))
                })?
        }
        Value::Table(t) => crate::lua::parsers::agent_from_lua_table(&t)?,
        _ => {
            return Err(mlua::Error::external(IronCrewError::Validation(
                "Conversation requires 'agent' (string name or Agent table)".into(),
            )));
        }
    };

    let model: String = table
        .get::<String>("model")
        .ok()
        .or_else(|| agent.model.clone())
        .unwrap_or_else(|| crew_default_model.to_string());

    let system_prompt: String = table
        .get::<String>("system_prompt")
        .ok()
        .or_else(|| agent.system_prompt.clone())
        .unwrap_or_else(|| format!("You are {}. Your goal: {}", agent.name, agent.goal));

    // max_history resolution order:
    //   1. Explicit positive value in the Lua table
    //   2. IRONCREW_CONVERSATION_MAX_HISTORY env var
    //   3. Safe default of 50 messages
    let max_history: Option<usize> = match table.get::<usize>("max_history") {
        Ok(n) if (1..=HARD_CHAT_HISTORY_MAX_MESSAGES).contains(&n) => Some(n),
        Ok(n) => {
            return Err(mlua::Error::external(IronCrewError::Validation(format!(
                "Conversation max_history must be between 1 and {HARD_CHAT_HISTORY_MAX_MESSAGES}, got {n}"
            ))));
        }
        Err(_) => default_max_history(),
    };

    let stream: bool = table.get::<bool>("stream").unwrap_or(false);

    // Cross-run persistence: `id` is the persistence key. When omitted,
    // the session is ephemeral (same behavior as pre-2.8 conversations).
    let id: Option<String> = table.get::<String>("id").ok();
    // Autosave defaults to true when persistence is active. For non-persistent
    // sessions this value is effectively ignored.
    //
    // NOTE: use `Option<bool>` rather than `bool` here — `table.get::<bool>`
    // on a missing key coerces nil to `false` (mlua's FromLua impl), which
    // would silently disable autosave whenever the caller omits the field.
    let autosave: bool = table
        .get::<Option<bool>>("autosave")
        .ok()
        .flatten()
        .unwrap_or(true);

    let effective_max_history = max_history.unwrap_or(DEFAULT_CHAT_HISTORY_MAX_MESSAGES);
    let history_max_bytes = tool_registry.chat_history_max_bytes();
    let provider_execution_fingerprint = match provider.execution_fingerprint() {
        Ok(fingerprint) => fingerprint,
        Err(_) if id.is_none() => {
            crate::engine::conversation_provider::unidentified_ephemeral_provider_fingerprint()
        }
        Err(error) => return Err(mlua::Error::external(error)),
    };
    let resolved_tools_fingerprint = tool_registry
        .conversation_execution_fingerprint(&agent.tools)
        .map_err(mlua::Error::external)?;
    let app_db_fingerprint = lua
        .app_data_ref::<crate::engine::conversation_definition::AppDbFingerprint>()
        .map(|data| data.0.clone());
    let definition_fingerprint = conversation_definition_fingerprint(&ConversationDefinition {
        source_fingerprint,
        agent: &agent,
        resolved_model: &model,
        effective_system_prompt: &system_prompt,
        max_history: effective_max_history,
        history_max_bytes,
        max_tool_rounds: crew_max_tool_rounds,
        resolved_tools_fingerprint: &resolved_tools_fingerprint,
        provider_execution_fingerprint: &provider_execution_fingerprint,
        app_db: app_db_fingerprint.as_ref(),
    })
    .map_err(mlua::Error::external)?;

    let inner = LuaConversationInner::new_or_resume(
        agent,
        provider,
        tool_registry,
        model,
        system_prompt,
        max_history,
        history_max_bytes,
        stream,
        crew_max_tool_rounds,
        eventbus,
        id,
        store,
        flow_name,
        flow_path,
        autosave,
        project_dir,
        http_client,
        source_fingerprint.to_string(),
        definition_fingerprint,
    )
    .await
    .map_err(mlua::Error::external)?;

    Ok(LuaConversation(Arc::new(inner)))
}
