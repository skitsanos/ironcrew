use super::*;

pub async fn cmd_run(
    path: &Path,
    input_json: Option<&str>,
    json_output: bool,
    tags: Vec<String>,
) -> Result<()> {
    let usage_tracker = crate::usage::UsageTracker::for_run()?;
    let loader = load_project(path)?;
    let (lua, _runtime) = setup_crew_runtime(&loader)?;
    lua.set_app_data(usage_tracker);

    // Sweep only expired ownership leases from prior crashed processes before
    // this invocation writes its own run intent.
    let ironcrew_dir = loader.project_dir().join(".ironcrew");
    let lease_store = if let Ok(store) = crate::engine::store::create_store(ironcrew_dir).await {
        let _ = crate::engine::reconciler::reconcile_stuck_runs(&store)
            .await
            .map_err(|e| {
                tracing::debug!("Reconciler failed (non-fatal): {e}");
            });
        Some(store)
    } else {
        None
    };

    // Human-input transport for crew:ask_human(): CLI mode prompts on the
    // terminal (stderr prompt, stdin answer). `run_id: None` — the run
    // record is created inside crew:run() and terminal prompting doesn't
    // need store status flips; non-TTY stdin resolves as immediate timeout.
    lua.set_app_data(crate::engine::input_bridge::AskHumanContext {
        bridge: std::sync::Arc::new(crate::engine::input_bridge::InputBridge::new(
            crate::engine::input_bridge::BridgeMode::Tty,
        )),
        // crew:run() re-binds run_id/store/eventbus with the real values it
        // allocates, so agent-initiated asks are fully wired even from here.
        run_id: None,
        store: None,
        eventbus: None,
    });

    // In --json mode, suppress Lua print() by marking via app_data
    if json_output {
        lua.set_app_data(JsonOutputMode);
    }

    // Store tags so LuaCrew::run() can attach them to the run record
    if !tags.is_empty() {
        lua.set_app_data(tags);
    }

    // Inject input as a global `input` table (from --input CLI flag)
    if let Some(json_str) = input_json {
        let value: serde_json::Value = serde_json::from_str(json_str)
            .map_err(|e| IronCrewError::Validation(format!("Invalid --input JSON: {}", e)))?;
        let lua_input =
            crate::lua::api::json_value_to_lua(&lua, &value).map_err(IronCrewError::Lua)?;
        lua.globals()
            .set("input", lua_input)
            .map_err(IronCrewError::Lua)?;
    }

    // Execute entrypoint
    let entrypoint = loader
        .entrypoint()
        .ok_or_else(|| IronCrewError::Validation("No entrypoint found".into()))?;
    let script = crate::lua::source::read_lua_source(entrypoint)?;

    tracing::info!("Running {}", entrypoint.display());

    let heartbeat_handle = lease_store.map(|heartbeat_store| {
        let heartbeat_interval =
            (heartbeat_store.run_lease_ttl() / 3).max(std::time::Duration::from_secs(1));
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(heartbeat_interval);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                interval.tick().await;
                if let Err(error) = heartbeat_store.heartbeat_owned_runs().await {
                    tracing::warn!(%error, "Failed to refresh CLI run leases");
                }
            }
        })
    });

    let execution_result = {
        let _execution = LuaExecutionGuard::begin(&lua).map_err(IronCrewError::Lua)?;
        lua.load(&script)
            .exec_async()
            .await
            .map_err(IronCrewError::Lua)
    };

    if let Some(handle) = heartbeat_handle {
        handle.abort();
    }
    execution_result?;
    crate::lua::usage::tracker(&lua)?.budget().check()?;

    // In --json mode, read the run record and output structured JSON
    if json_output {
        let run_id: Option<String> = lua.globals().get("__ironcrew_last_run_id").ok();
        if let Some(run_id) = run_id {
            let ironcrew_dir = loader.project_dir().join(".ironcrew");
            if let Ok(store) = crate::engine::store::create_store(ironcrew_dir).await
                && let Ok(record) = store.get_run(&run_id).await
            {
                let json = serde_json::to_string_pretty(&record).unwrap_or_else(|_| "{}".into());
                println!("{}", json);
                return Ok(());
            }
        }
    }

    Ok(())
}
