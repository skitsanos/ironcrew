use mlua::{Result as LuaResult, Table};

pub(crate) const CONVERSATION_KEYS: &[&str] = &[
    "agent",
    "model",
    "system_prompt",
    "max_history",
    "stream",
    "id",
    "autosave",
];

pub(crate) const DIALOG_KEYS: &[&str] = &[
    "agents",
    "starter",
    "max_turns",
    "max_history",
    "stream",
    "starting_speaker",
    "model",
    "turn_selector",
    "should_stop",
    "id",
    "autosave",
];

pub(crate) fn reject_conversation_keys(table: &Table) -> LuaResult<()> {
    super::reject_unknown_keys(table, CONVERSATION_KEYS, "Conversation")
}

pub(crate) fn reject_dialog_keys(table: &Table) -> LuaResult<()> {
    super::reject_unknown_keys(table, DIALOG_KEYS, "Dialog")
}
