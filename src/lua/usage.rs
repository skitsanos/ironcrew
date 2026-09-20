//! One process-local scope per executing Lua VM, explicitly inherited by child VMs.
use crate::llm::provider::LlmProvider;
use crate::usage::UsageTracker;
use mlua::serde::SerializeOptions;
use mlua::{LuaSerdeExt, Value};
use std::sync::Arc;

pub(crate) fn tracker(lua: &mlua::Lua) -> UsageTracker {
    if let Some(tracker) = lua.app_data_ref::<UsageTracker>() {
        return tracker.clone();
    }
    let tracker = UsageTracker::default();
    lua.set_app_data(tracker.clone());
    tracker
}

pub(crate) fn bind(
    lua: &mlua::Lua,
    provider: Arc<dyn LlmProvider>,
) -> mlua::Result<Arc<dyn LlmProvider>> {
    let child = tracker(lua).child().map_err(mlua::Error::external)?;
    Ok(crate::llm::scope::with_usage_tracker(provider, child))
}

pub(crate) fn snapshot(lua: &mlua::Lua, tracker: &UsageTracker) -> mlua::Result<Value> {
    let snapshot = tracker.snapshot().map_err(mlua::Error::external)?;
    lua.to_value_with(
        &snapshot,
        SerializeOptions::new().serialize_none_to_null(true),
    )
}

pub(crate) fn provider_snapshot(
    lua: &mlua::Lua,
    provider: &dyn LlmProvider,
) -> mlua::Result<Value> {
    let tracker = provider
        .usage_tracker()
        .ok_or_else(|| mlua::Error::external("provider has no execution usage scope"))?;
    snapshot(lua, &tracker)
}
