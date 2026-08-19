//! Host-enforced purity boundary for HTTP conversation construction.
//!
//! An HTTP conversation must evaluate its entrypoint to discover the
//! declarative `Crew` definition before it can create or rehydrate the real
//! conversation handle. Re-running that discovery after eviction or on a
//! different replica must not repeat network, provider, sub-flow, or
//! filesystem effects.

use mlua::{Lua, Result as LuaResult};

use crate::utils::error::IronCrewError;

/// Marker installed only while the HTTP conversation handler evaluates the
/// flow entrypoint to discover its `Crew` definition.
#[derive(Clone, Copy, Debug)]
pub(crate) struct HttpConversationBootstrap;

/// Marker installed while `config.lua` is evaluated. Configuration is a
/// declarative defaults table, not an execution phase, so effectful
/// capabilities must fail before starting physical work.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ConfigEvaluation;

/// Reject an effectful Lua capability before it starts physical work while
/// the HTTP conversation bootstrap marker is installed.
pub(crate) fn reject_effect(lua: &Lua, capability: &str) -> LuaResult<()> {
    if lua.app_data_ref::<ConfigEvaluation>().is_some() {
        return Err(mlua::Error::external(IronCrewError::Validation(format!(
            "Lua capability '{capability}' is unavailable during config.lua evaluation; config.lua may only return declarative Crew defaults"
        ))));
    }
    if lua.app_data_ref::<HttpConversationBootstrap>().is_some() {
        return Err(mlua::Error::external(IronCrewError::Validation(format!(
            "Lua capability '{capability}' is unavailable during HTTP conversation bootstrap; the entrypoint may only construct declarative Crew, Agent, and task definitions"
        ))));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{ConfigEvaluation, HttpConversationBootstrap, reject_effect};
    use crate::lua::sandbox::create_tool_lua_with_base_dir;

    #[test]
    fn marker_is_scoped_and_capability_names_are_reported() {
        let lua = mlua::Lua::new();
        reject_effect(&lua, "http.get").unwrap();

        lua.set_app_data(HttpConversationBootstrap);
        let error = reject_effect(&lua, "http.get").unwrap_err().to_string();
        assert!(error.contains("http.get"));
        assert!(error.contains("HTTP conversation bootstrap"));

        assert!(lua.remove_app_data::<HttpConversationBootstrap>().is_some());
        reject_effect(&lua, "http.get").unwrap();

        lua.set_app_data(ConfigEvaluation);
        let error = reject_effect(&lua, "postgres.query")
            .unwrap_err()
            .to_string();
        assert!(error.contains("postgres.query"));
        assert!(error.contains("config.lua evaluation"));
        assert!(lua.remove_app_data::<ConfigEvaluation>().is_some());
    }

    #[tokio::test]
    async fn filesystem_write_is_rejected_before_creation_and_restored_after_scope() {
        let directory = tempfile::tempdir().unwrap();
        let lua = create_tool_lua_with_base_dir(Some(directory.path().to_path_buf())).unwrap();
        let output = directory.path().join("bootstrap.txt");

        lua.set_app_data(HttpConversationBootstrap);
        let error = lua
            .load("fs.write('bootstrap.txt', 'blocked')")
            .exec_async()
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("fs.write"), "unexpected error: {error}");
        assert!(!output.exists(), "blocked fs.write created a file");

        lua.remove_app_data::<HttpConversationBootstrap>();
        lua.load("fs.write('bootstrap.txt', 'allowed')")
            .exec_async()
            .await
            .unwrap();
        assert_eq!(std::fs::read_to_string(output).unwrap(), "allowed");
    }
}
