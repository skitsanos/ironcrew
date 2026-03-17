use ironcrew::lua::api::*;
use ironcrew::lua::sandbox::create_crew_lua;
use mlua::Table;

#[test]
fn test_agent_from_lua_table() {
    let lua = create_crew_lua().unwrap();
    let table: Table = lua
        .load(
            r#"
        return {
            name = "researcher",
            goal = "Research topics",
            capabilities = {"research", "analysis"},
            tools = {"web_scrape"},
            temperature = 0.3,
        }
        "#,
        )
        .eval()
        .unwrap();

    let agent = agent_from_lua_table(&table).unwrap();
    assert_eq!(agent.name, "researcher");
    assert_eq!(agent.goal, "Research topics");
    assert_eq!(agent.capabilities, vec!["research", "analysis"]);
    assert_eq!(agent.tools, vec!["web_scrape"]);
    assert_eq!(agent.temperature, Some(0.3));
    assert!(agent.max_tokens.is_none());
}

#[test]
fn test_task_from_lua_table() {
    let lua = create_crew_lua().unwrap();
    let table: Table = lua
        .load(
            r#"
        return {
            name = "research",
            description = "Research AI trends",
            depends_on = {"setup"},
            expected_output = "A summary",
        }
        "#,
        )
        .eval()
        .unwrap();

    let task = task_from_lua_table(&table).unwrap();
    assert_eq!(task.name, "research");
    assert_eq!(task.description, "Research AI trends");
    assert_eq!(task.depends_on, vec!["setup"]);
    assert_eq!(task.expected_output, Some("A summary".into()));
}

#[test]
fn test_response_format_json_schema() {
    let lua = create_crew_lua().unwrap();
    let table: Table = lua
        .load(
            r#"
        return {
            name = "extractor",
            goal = "Extract data",
            response_format = {
                type = "json_schema",
                name = "result",
                schema = {
                    type = "object",
                    properties = {
                        items = { type = "array" },
                    },
                    required = {"items"},
                },
            },
        }
        "#,
        )
        .eval()
        .unwrap();

    let agent = agent_from_lua_table(&table).unwrap();
    assert!(agent.response_format.is_some());
}

#[test]
fn test_env_function() {
    let lua = create_crew_lua().unwrap();
    register_env_function(&lua).unwrap();

    unsafe { std::env::set_var("TEST_IRONCREW_VAR", "hello") };
    let result: Option<String> = lua
        .load(r#"return env("TEST_IRONCREW_VAR")"#)
        .eval()
        .unwrap();
    assert_eq!(result, Some("hello".into()));

    let result: Option<String> = lua
        .load(r#"return env("NONEXISTENT_VAR_XYZ")"#)
        .eval()
        .unwrap();
    assert_eq!(result, None);
    unsafe { std::env::remove_var("TEST_IRONCREW_VAR") };
}

#[test]
fn test_lua_table_to_json() {
    let lua = create_crew_lua().unwrap();
    let table: Table = lua
        .load(
            r#"
        return {
            type = "object",
            properties = {
                name = { type = "string" },
                count = { type = "number" },
            },
            required = {"name"},
        }
        "#,
        )
        .eval()
        .unwrap();

    let json = lua_table_to_json(&table).unwrap();
    assert_eq!(json["type"], "object");
    assert!(json["properties"]["name"]["type"] == "string");
}
