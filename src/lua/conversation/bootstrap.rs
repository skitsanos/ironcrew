use super::*;

impl LuaConversationInner {
    /// Build a fresh (or resumed) conversation inner.
    ///
    /// When `store` is `Some` and `id` is `Some`, the store is consulted for
    /// a prior record with that id. On hit, the persisted history replaces
    /// the freshly-seeded `[system]` bootstrap so the conversation picks up
    /// where it left off. On miss, a new record will be written on the
    /// first autosave.
    #[allow(clippy::too_many_arguments)]
    pub async fn new_or_resume(
        agent: Agent,
        provider: Arc<dyn LlmProvider>,
        tool_registry: ToolRegistry,
        model: String,
        system_prompt: String,
        max_history: Option<usize>,
        history_max_bytes: usize,
        stream: bool,
        max_tool_rounds: usize,
        eventbus: EventBus,
        id: Option<String>,
        store: Option<Arc<dyn StateStore>>,
        flow_name: String,
        flow_path: Option<String>,
        autosave: bool,
        project_dir: std::path::PathBuf,
        http_client: reqwest::Client,
        source_fingerprint: String,
        definition_fingerprint: String,
    ) -> Result<Self, IronCrewError> {
        let provider = crate::llm::scope::ensure_scope(provider)?;
        provider.validate_request(
            &agent.chat_request(model.clone(), vec![]),
            !tool_registry.schemas_for(&agent.tools).is_empty(),
        )?;
        let now = chrono::Utc::now().to_rfc3339();
        let max_history = match max_history {
            Some(value) if (1..=HARD_CHAT_HISTORY_MAX_MESSAGES).contains(&value) => value,
            Some(value) => {
                return Err(IronCrewError::Validation(format!(
                    "max_history must be between 1 and {HARD_CHAT_HISTORY_MAX_MESSAGES}, got {value}"
                )));
            }
            None => DEFAULT_CHAT_HISTORY_MAX_MESSAGES,
        };
        // Resolve the id and decide whether the session is persistent.
        let (id, persistent) = match id {
            Some(s) => {
                validate_session_id(&s)?;
                (s, true)
            }
            None => (uuid::Uuid::new_v4().to_string(), false),
        };

        // Seed the message list. If we can hit the store for a resume, use
        // the persisted messages instead of the bootstrap seed.
        let mut messages = vec![ChatMessage::system(&system_prompt)];
        let mut created_at = now.clone();
        let mut revision = 0;
        let mut usage = crate::usage::UsageTracker::default();
        let mut execution = ConversationExecution::new(
            source_fingerprint.clone(),
            definition_fingerprint.clone(),
            max_history,
            history_max_bytes,
        )?;

        if persistent
            && let Some(ref store) = store
            && let Some(record) = store.get_conversation(flow_path.as_deref(), &id).await?
        {
            record.execution.validate()?;
            if record.execution.source_fingerprint != source_fingerprint {
                return Err(IronCrewError::Conflict(
                    "Conversation flow source changed; restore the original definition or start a new conversation"
                        .into(),
                ));
            }
            if record.execution.definition_fingerprint != definition_fingerprint {
                return Err(IronCrewError::Conflict(
                    "Conversation definition changed; restore the original model, agent, tools, provider, and limits or start a new conversation"
                        .into(),
                ));
            }
            validate_chat_history(&record.messages, max_history, history_max_bytes, true).map_err(
                |error| {
                    IronCrewError::Validation(format!(
                        "Conversation '{id}' has invalid persisted history: {error}"
                    ))
                },
            )?;
            revision = record.revision;
            usage = crate::usage::UsageTracker::from_snapshot(record.usage)
                .map_err(|error| IronCrewError::Validation(error.into()))?;
            execution = record.execution;
            messages = record.messages;
            created_at = record.created_at;
            tracing::info!(
                "Resumed conversation '{}' with {} messages",
                id,
                messages.len()
            );
        }

        validate_chat_history(&messages, max_history, history_max_bytes, true)?;

        eventbus.emit(CrewEvent::ConversationStarted {
            conversation_id: id.clone(),
            agent: agent.name.clone(),
        });

        let provider = crate::llm::scope::observe_session(provider, &usage)?;
        Ok(Self {
            usage,
            id,
            persistent,
            agent,
            provider,
            tool_registry,
            model,
            system_prompt,
            messages: Mutex::new(messages),
            turn_execution_lock: Arc::new(Mutex::new(())),
            max_history: Some(max_history),
            history_max_bytes,
            stream,
            max_tool_rounds,
            eventbus,
            store: if persistent { store } else { None },
            flow_name,
            flow_path,
            autosave,
            created_at,
            revision: Mutex::new(revision),
            execution,
            project_dir,
            http_client,
        })
    }
}
