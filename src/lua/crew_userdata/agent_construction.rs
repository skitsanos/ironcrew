use super::*;

pub(super) fn register<M: UserDataMethods<LuaCrew>>(methods: &mut M) {
    // Use add_async_method for all methods to avoid block_on inside Tokio
    methods.add_async_method("add_agent", |_, this, table: Table| async move {
        let agent = agent_from_lua_table(&table)?;
        let agent_name = agent.name.clone();

        let mut crew = this.crew.lock().await;
        let provider = this
            .custom_provider
            .as_ref()
            .unwrap_or(&this.runtime.provider);
        crate::lua::provider_validation::agent(provider.as_ref(), &agent, &crew)
            .map_err(mlua::Error::external)?;

        // Validate uniqueness/count before mutating hook maps so a
        // rejected agent leaves the existing crew unchanged.
        crew.add_agent(agent).map_err(mlua::Error::external)?;

        // Extract before_task hook if present and store as bytecode
        if let Ok(func) = table.get::<mlua::Function>("before_task") {
            let bytecode = func.dump(false);
            crew.before_task_hooks.insert(agent_name.clone(), bytecode);
        }

        // Extract after_task hook if present and store as bytecode
        if let Ok(func) = table.get::<mlua::Function>("after_task") {
            let bytecode = func.dump(false);
            crew.after_task_hooks.insert(agent_name.clone(), bytecode);
        }

        Ok(())
    });
}
