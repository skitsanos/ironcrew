use mlua::{AnyUserData, MetaMethod, MultiValue, ObjectLike, Table, UserData, UserDataMethods};

use crate::lua::crew_userdata::LuaCrew;

pub(super) struct ConstructionCrew(pub AnyUserData);

impl UserData for ConstructionCrew {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        for name in [
            "add_agent",
            "add_task",
            "add_foreach_task",
            "add_collaborative_task",
        ] {
            methods.add_async_method(name, move |lua, this, table: Table| async move {
                super::budget::admit(&lua, &table)?;
                this.0.call_async_method::<()>(name, table).await
            });
        }
        methods.add_async_method(
            "add_task_if",
            |lua, this, (condition, table): (String, Table)| async move {
                super::budget::admit(&lua, &table)?;
                this.0
                    .call_async_method::<()>("add_task_if", (condition, table))
                    .await
            },
        );
        methods.add_async_method("conversation", |lua, this, table: Table| async move {
            super::budget::admit(&lua, &table)?;
            let original = this.0.borrow::<LuaCrew>()?;
            let registry = super::references::registry(&lua, &original)
                .await
                .map_err(mlua::Error::external)?;
            let crew = original.crew.lock().await;
            let provider = original
                .custom_provider
                .as_ref()
                .unwrap_or(&original.runtime.provider)
                .clone();
            let fingerprint = lua
                .app_data_ref::<crate::engine::conversation_definition::ConversationSourceContext>()
                .expect("validation snapshot installed")
                .snapshot
                .fingerprint()
                .to_owned();
            // Real builder, but no store and no resulting executable handle
            // escapes. Resume state and session methods remain unavailable.
            crate::lua::conversation::build_conversation(
                &lua,
                table,
                &crew.agents,
                provider,
                registry,
                &crew.provider_config.model,
                &fingerprint,
                crew.max_tool_rounds,
                crew.eventbus.clone(),
                None,
                crew.goal.clone(),
                None,
                original.project_dir.clone(),
                reqwest::Client::new(),
            )
            .await?;
            Ok(ConstructionSession("conversation"))
        });
        methods.add_async_method("dialog", |lua, this, table: Table| async move {
            super::budget::admit(&lua, &table)?;
            let original = this.0.borrow::<LuaCrew>()?;
            let registry = super::references::registry(&lua, &original)
                .await
                .map_err(mlua::Error::external)?;
            let crew = original.crew.lock().await;
            let provider = original
                .custom_provider
                .as_ref()
                .unwrap_or(&original.runtime.provider)
                .clone();
            crate::lua::dialog::build_dialog(
                &lua,
                table,
                &crew.agents,
                provider,
                registry,
                &crew.provider_config.model,
                crew.max_tool_rounds,
                crew.eventbus.clone(),
                None,
                crew.goal.clone(),
                None,
            )
            .await?;
            Ok(ConstructionSession("dialog"))
        });
        methods.add_meta_method(
            MetaMethod::Index,
            |lua, _, _: MultiValue| -> mlua::Result<()> {
                Err(super::block(lua, "crew execution/state method"))
            },
        );
    }
}

struct ConstructionSession(&'static str);

impl UserData for ConstructionSession {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_meta_method(
            MetaMethod::Index,
            |lua, this, _: MultiValue| -> mlua::Result<()> { Err(super::block(lua, this.0)) },
        );
    }
}
