use super::*;
use mlua::Lua;
use std::path::Path;

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
fn agent_from_lua_table_rejects_malformed_agent_tool_name() {
    let lua = Lua::new();
    let table = base_agent(&lua);
    let tools = lua.create_sequence_from(vec!["agent__BadCase"]).unwrap();
    table.set("tools", tools).unwrap();

    let result = agent_from_lua_table(&table);
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("agent__BadCase"),
        "error should mention the malformed name, got: {err}"
    );
}

#[test]
fn agent_from_lua_table_accepts_valid_agent_tool_name() {
    let lua = Lua::new();
    let table = base_agent(&lua);
    let tools = lua.create_sequence_from(vec!["agent__researcher"]).unwrap();
    table.set("tools", tools).unwrap();

    let result = agent_from_lua_table(&table);
    assert!(
        result.is_ok(),
        "valid agent tool name rejected: {:?}",
        result.err()
    );
}

#[test]
fn agent_rejects_invalid_scalar_limits() {
    let lua = Lua::new();

    let table = base_agent(&lua);
    table.set("name", "bad\nname").unwrap();
    assert!(
        agent_from_lua_table(&table)
            .unwrap_err()
            .to_string()
            .contains("control")
    );

    let table = base_agent(&lua);
    table.set("temperature", f64::NAN).unwrap();
    assert!(
        agent_from_lua_table(&table)
            .unwrap_err()
            .to_string()
            .contains("temperature")
    );

    let table = base_agent(&lua);
    table.set("max_tokens", 0).unwrap();
    assert!(
        agent_from_lua_table(&table)
            .unwrap_err()
            .to_string()
            .contains("max_tokens")
    );

    let table = base_agent(&lua);
    table
        .set("system_prompt", "x".repeat(MAX_TEXT_BYTES + 1))
        .unwrap();
    assert!(
        agent_from_lua_table(&table)
            .unwrap_err()
            .to_string()
            .contains("system_prompt")
    );
}

#[test]
fn agent_lists_are_strict_contiguous_and_bounded() {
    let lua = Lua::new();

    let table = base_agent(&lua);
    let tools = lua.create_table().unwrap();
    tools.set(1, "file_read").unwrap();
    tools.set(2, true).unwrap();
    table.set("tools", tools).unwrap();
    assert!(
        agent_from_lua_table(&table)
            .unwrap_err()
            .to_string()
            .contains("must be a string")
    );

    let table = base_agent(&lua);
    let capabilities = lua.create_table().unwrap();
    capabilities.set(2, "analysis").unwrap();
    table.set("capabilities", capabilities).unwrap();
    assert!(
        agent_from_lua_table(&table)
            .unwrap_err()
            .to_string()
            .contains("contiguous")
    );

    let table = base_agent(&lua);
    let capabilities = lua.create_table().unwrap();
    for index in 1..=MAX_LIST_ITEMS + 1 {
        capabilities.set(index, "analysis").unwrap();
    }
    table.set("capabilities", capabilities).unwrap();
    assert!(
        agent_from_lua_table(&table)
            .unwrap_err()
            .to_string()
            .contains("maximum")
    );

    let table = base_agent(&lua);
    let tools = lua.create_sequence_from(["not.provider.safe"]).unwrap();
    table.set("tools", tools).unwrap();
    assert!(
        agent_from_lua_table(&table)
            .unwrap_err()
            .to_string()
            .contains("ASCII")
    );
}

#[test]
fn response_schema_name_and_size_are_bounded() {
    let lua = Lua::new();

    let table = base_agent(&lua);
    let format = lua.create_table().unwrap();
    format.set("type", "json_schema").unwrap();
    format.set("name", "invalid.schema").unwrap();
    format.set("schema", lua.create_table().unwrap()).unwrap();
    table.set("response_format", format).unwrap();
    assert!(
        agent_from_lua_table(&table)
            .unwrap_err()
            .to_string()
            .contains("ASCII")
    );

    let table = base_agent(&lua);
    let format = lua.create_table().unwrap();
    format.set("type", "json_schema").unwrap();
    format.set("name", "valid_schema").unwrap();
    let schema = lua.create_table().unwrap();
    schema
        .set("description", "x".repeat(MAX_SCHEMA_BYTES + 1))
        .unwrap();
    format.set("schema", schema).unwrap();
    table.set("response_format", format).unwrap();
    assert!(
        agent_from_lua_table(&table)
            .unwrap_err()
            .to_string()
            .contains("serialized size")
    );
}

#[test]
fn task_rejects_bad_names_lists_and_boolean_fields() {
    let lua = Lua::new();

    let table = base_task(&lua);
    table.set("name", "").unwrap();
    assert!(
        task_from_lua_table(&table)
            .unwrap_err()
            .to_string()
            .contains("must not be empty")
    );

    let table = base_task(&lua);
    let dependencies = lua.create_table().unwrap();
    dependencies.set(1, "setup").unwrap();
    dependencies.set(3, "missing-middle").unwrap();
    table.set("depends_on", dependencies).unwrap();
    assert!(
        task_from_lua_table(&table)
            .unwrap_err()
            .to_string()
            .contains("contiguous")
    );

    let table = base_task(&lua);
    table.set("stream", "yes").unwrap();
    assert!(
        task_from_lua_table(&table)
            .unwrap_err()
            .to_string()
            .contains("boolean")
    );
}

#[test]
fn custom_tool_name_and_parameter_definitions_are_strict() {
    let lua = Lua::new();
    let table = lua.create_table().unwrap();
    table.set("name", "bad.tool").unwrap();
    table.set("description", "description").unwrap();
    table
        .set("parameters", lua.create_table().unwrap())
        .unwrap();
    table
        .set("execute", lua.create_function(|_, ()| Ok(())).unwrap())
        .unwrap();
    assert!(
        tool_def_from_lua_table(&table, Path::new("tool.lua"), Arc::from("return {}"))
            .err()
            .unwrap()
            .to_string()
            .contains("ASCII")
    );

    table.set("name", "valid_tool").unwrap();
    let parameters = lua.create_table().unwrap();
    parameters.set("query", "not a table").unwrap();
    table.set("parameters", parameters).unwrap();
    assert!(
        tool_def_from_lua_table(&table, Path::new("tool.lua"), Arc::from("return {}"))
            .err()
            .unwrap()
            .to_string()
            .contains("definition must be an object")
    );
}
