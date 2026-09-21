//! One process-local scope per executing Lua VM, explicitly inherited by child VMs.
use crate::llm::provider::LlmProvider;
use crate::usage::UsageTracker;
use mlua::serde::SerializeOptions;
use mlua::{LuaSerdeExt, Value};
use std::sync::Arc;

pub(crate) fn tracker(lua: &mlua::Lua) -> mlua::Result<UsageTracker> {
    if let Some(tracker) = lua.app_data_ref::<UsageTracker>() {
        return Ok(tracker.clone());
    }
    let tracker = UsageTracker::for_run().map_err(mlua::Error::external)?;
    lua.set_app_data(tracker.clone());
    Ok(tracker)
}

pub(crate) fn bind(
    lua: &mlua::Lua,
    provider: Arc<dyn LlmProvider>,
) -> mlua::Result<Arc<dyn LlmProvider>> {
    let child = tracker(lua)?.child().map_err(mlua::Error::external)?;
    Ok(crate::llm::scope::with_usage_tracker(provider, child))
}

pub(crate) fn snapshot(lua: &mlua::Lua, tracker: &UsageTracker) -> mlua::Result<Value> {
    let snapshot = tracker.snapshot().map_err(mlua::Error::external)?;
    lua.to_value_with(
        &snapshot,
        SerializeOptions::new().serialize_none_to_null(true),
    )
}
