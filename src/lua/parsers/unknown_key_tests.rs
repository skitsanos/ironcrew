//! Unknown-key rejection for the Lua configuration surface (IC-028).

use mlua::{Lua, Table};

use super::option_keys::{CONVERSATION_KEYS, DIALOG_KEYS};
use super::{AGENT_KEYS, agent_from_lua_table, reject_unknown_keys, task_from_lua_table};

fn base_agent(lua: &Lua) -> Table {
    let table = lua.create_table().unwrap();
    table.set("name", "coordinator").unwrap();
    table.set("goal", "route asks").unwrap();
    table
}

fn base_task(lua: &Lua) -> Table {
    let table = lua.create_table().unwrap();
    table.set("name", "research").unwrap();
    table.set("description", "Research the topic").unwrap();
    table
}

#[test]
fn agent_table_rejects_unknown_keys() {
    let lua = Lua::new();
    let table = base_agent(&lua);
    // CrewAI-style fields the runtime never reads.
    table.set("backstory", "a careful tester").unwrap();
    table.set("max_iter", 3).unwrap();

    let error = agent_from_lua_table(&table)
        .expect_err("unknown agent options must be rejected, not ignored")
        .to_string();
    assert!(error.contains("backstory"), "unexpected error: {error}");
    assert!(error.contains("max_iter"), "unexpected error: {error}");
    // The message must tell the author what is actually supported.
    assert!(error.contains("system_prompt"), "unexpected error: {error}");
}

#[test]
fn agent_table_accepts_every_documented_key() {
    let lua = Lua::new();
    let table = base_agent(&lua);
    table.set("expected_output", "a summary").unwrap();
    table.set("system_prompt", "be terse").unwrap();
    table.set("temperature", 0.5).unwrap();
    table.set("max_tokens", 256).unwrap();
    table.set("model", "gpt-x").unwrap();
    table
        .set(
            "capabilities",
            lua.create_sequence_from(vec!["research"]).unwrap(),
        )
        .unwrap();
    table
        .set(
            "tools",
            lua.create_sequence_from(vec!["file_read"]).unwrap(),
        )
        .unwrap();

    agent_from_lua_table(&table).expect("all documented agent options must be accepted");
}

#[test]
fn task_table_rejects_unknown_keys() {
    let lua = Lua::new();
    let table = base_task(&lua);
    table.set("dependson", lua.create_table().unwrap()).unwrap();

    let error = task_from_lua_table(&table)
        .expect_err("a misspelled task option must be rejected")
        .to_string();
    assert!(error.contains("dependson"), "unexpected error: {error}");
    assert!(error.contains("depends_on"), "unexpected error: {error}");
}

#[test]
fn unknown_key_check_ignores_array_parts_and_internal_keys() {
    let lua = Lua::new();
    let table = base_agent(&lua);
    table.set(1, "positional").unwrap();
    table.set("__ironcrew_internal", true).unwrap();

    reject_unknown_keys(&table, AGENT_KEYS, "agent")
        .expect("array parts and internal keys are not author options");
}

#[test]
fn conversation_and_dialog_tables_are_closed_sets() {
    let lua = Lua::new();
    let conversation = lua.create_table().unwrap();
    conversation.set("agent", "writer").unwrap();
    conversation.set("max_hstory", 10).unwrap();
    let error = reject_unknown_keys(&conversation, CONVERSATION_KEYS, "Conversation")
        .unwrap_err()
        .to_string();
    assert!(error.contains("max_hstory"), "{error}");
    assert!(error.contains("max_history"), "{error}");

    let dialog = lua.create_table().unwrap();
    dialog.set("agents", lua.create_table().unwrap()).unwrap();
    dialog.set("starting_speker", "a").unwrap();
    let error = reject_unknown_keys(&dialog, DIALOG_KEYS, "Dialog")
        .unwrap_err()
        .to_string();
    assert!(error.contains("starting_speker"), "{error}");
    assert!(error.contains("starting_speaker"), "{error}");
}
