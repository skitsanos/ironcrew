//! `require` implementation backed exclusively by a conversation snapshot.

use mlua::{Lua, Result as LuaResult, Table, Value};

use crate::engine::conversation_definition::ConversationSourceContext;

use super::require::module_name_to_relpath;

const LOADED_KEY: &str = "__ic_snapshot_modules_loaded";
const LOADING_KEY: &str = "__ic_snapshot_modules_loading";

pub fn install_snapshot_require(lua: &Lua, context: ConversationSourceContext) -> LuaResult<()> {
    lua.set_named_registry_value(LOADED_KEY, lua.create_table()?)?;
    lua.set_named_registry_value(LOADING_KEY, lua.create_table()?)?;

    let require = lua.create_function(move |lua, name: String| {
        let module = module_name_to_relpath(&name).map_err(mlua::Error::external)?;
        let loaded: Table = lua.named_registry_value(LOADED_KEY)?;
        let cached: Value = loaded.get(name.as_str())?;
        if cached != Value::Nil {
            return Ok(cached);
        }

        let loading: Table = lua.named_registry_value(LOADING_KEY)?;
        if loading.get::<Value>(name.as_str())? != Value::Nil {
            return Err(mlua::Error::external(format!(
                "circular require detected: '{name}'"
            )));
        }

        let relative = context.logical_dir().join("_lib").join(module);
        let source = context
            .snapshot
            .source(&relative)
            .map_err(mlua::Error::external)?
            .ok_or_else(|| {
                mlua::Error::external(format!("module '{name}' not found in _lib snapshot"))
            })?;

        loading.set(name.as_str(), true)?;
        let outcome = lua
            .load(source.source())
            .set_name(format!("@snapshot/{}", source.relative_path().display()))
            .eval::<Value>();
        loading.set(name.as_str(), Value::Nil)?;

        let value = outcome?;
        let to_cache = if value == Value::Nil {
            Value::Boolean(true)
        } else {
            value
        };
        loaded.set(name.as_str(), to_cache.clone())?;
        Ok(to_cache)
    })?;
    lua.globals().set("require", require)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::Arc;

    use tempfile::TempDir;

    use super::*;
    use crate::engine::conversation_definition::capture_flow_source;

    #[cfg(unix)]
    #[test]
    fn module_is_read_from_snapshot_after_live_swap_back() {
        let dir = TempDir::new().unwrap();
        fs::create_dir(dir.path().join("_lib")).unwrap();
        fs::write(dir.path().join("crew.lua"), "return true").unwrap();
        let module = dir.path().join("_lib/value.lua");
        fs::write(&module, "return 'captured'").unwrap();
        let snapshot = Arc::new(capture_flow_source(dir.path()).unwrap());
        fs::write(&module, "return 'replacement'").unwrap();
        fs::write(&module, "return 'captured'").unwrap();

        let lua = Lua::new();
        install_snapshot_require(&lua, ConversationSourceContext::root(snapshot)).unwrap();
        let value: String = lua.load("return require('value')").eval().unwrap();
        assert_eq!(value, "captured");
    }
}
