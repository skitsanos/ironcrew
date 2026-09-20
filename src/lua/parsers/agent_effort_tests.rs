//! Per-agent `reasoning_effort`: accepted values are validated at parse time.

use mlua::{Lua, Table};

use super::agent_from_lua_table;

fn agent_table(lua: &Lua, effort: &str) -> Table {
    let table = lua.create_table().unwrap();
    table.set("name", "analyst").unwrap();
    table.set("goal", "analyze deeply").unwrap();
    table.set("reasoning_effort", effort).unwrap();
    table
}

#[test]
fn agent_accepts_a_documented_reasoning_effort() {
    let lua = Lua::new();
    let agent = agent_from_lua_table(&agent_table(&lua, "high")).unwrap();
    assert_eq!(agent.reasoning_effort.as_deref(), Some("high"));
}

#[test]
fn agent_rejects_a_misspelled_reasoning_effort_at_parse_time() {
    let lua = Lua::new();
    let error = agent_from_lua_table(&agent_table(&lua, "hgih"))
        .unwrap_err()
        .to_string();
    assert!(error.contains("agent.reasoning_effort"), "{error}");
    assert!(error.contains("'hgih'"), "{error}");
    assert!(error.contains("low, medium, high"), "{error}");
}
