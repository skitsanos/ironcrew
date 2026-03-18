use ironcrew::lua::api::*;
use ironcrew::lua::sandbox::{create_crew_lua, create_tool_lua};
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

// ---------------------------------------------------------------------------
// Lua global utility function tests
// ---------------------------------------------------------------------------

#[test]
fn test_uuid4_returns_valid_uuid() {
    let lua = create_crew_lua().unwrap();
    let result: String = lua.load(r#"return uuid4()"#).eval().unwrap();
    assert_eq!(result.len(), 36);
    assert_eq!(result.chars().filter(|c| *c == '-').count(), 4);
}

#[test]
fn test_now_rfc3339_returns_timestamp() {
    let lua = create_crew_lua().unwrap();
    let result: String = lua.load(r#"return now_rfc3339()"#).eval().unwrap();
    assert!(result.contains('T'));
    assert!(result.len() > 20);
}

#[test]
fn test_now_unix_ms_returns_positive_number() {
    let lua = create_crew_lua().unwrap();
    let result: i64 = lua.load(r#"return now_unix_ms()"#).eval().unwrap();
    assert!(result > 0);
}

#[test]
fn test_json_parse_stringify_roundtrip() {
    let lua = create_crew_lua().unwrap();
    let result: String = lua
        .load(
            r#"
        local obj = json_parse('{"name":"alice","age":30,"active":true}')
        return json_stringify(obj)
        "#,
        )
        .eval()
        .unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
    assert_eq!(parsed["name"], "alice");
    assert_eq!(parsed["age"], 30);
    assert_eq!(parsed["active"], true);
}

#[test]
fn test_json_parse_array() {
    let lua = create_crew_lua().unwrap();
    let result: String = lua
        .load(
            r#"
        local arr = json_parse('[1,2,3]')
        return json_stringify(arr)
        "#,
        )
        .eval()
        .unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
    assert_eq!(parsed, serde_json::json!([1, 2, 3]));
}

#[test]
fn test_base64_encode_decode_roundtrip() {
    let lua = create_crew_lua().unwrap();
    let result: String = lua
        .load(
            r#"
        local encoded = base64_encode("hello world")
        return base64_decode(encoded)
        "#,
        )
        .eval()
        .unwrap();
    assert_eq!(result, "hello world");
}

#[test]
fn test_base64_encode_known_value() {
    let lua = create_crew_lua().unwrap();
    let result: String = lua
        .load(r#"return base64_encode("hello")"#)
        .eval()
        .unwrap();
    assert_eq!(result, "aGVsbG8=");
}

#[test]
fn test_log_does_not_crash() {
    let lua = create_crew_lua().unwrap();
    lua.load(r#"log("info", "test message from lua")"#)
        .exec()
        .unwrap();
    lua.load(r#"log("just a message")"#).exec().unwrap();
    lua.load(r#"log("debug", "multi", "part", "message")"#)
        .exec()
        .unwrap();
}

#[test]
fn test_globals_available_in_tool_lua() {
    let lua = create_tool_lua().unwrap();
    // Verify all globals are available in tool context too
    let uuid: String = lua.load(r#"return uuid4()"#).eval().unwrap();
    assert_eq!(uuid.len(), 36);

    let ts: i64 = lua.load(r#"return now_unix_ms()"#).eval().unwrap();
    assert!(ts > 0);

    let encoded: String = lua
        .load(r#"return base64_encode("test")"#)
        .eval()
        .unwrap();
    let decoded: String = lua
        .load(format!(r#"return base64_decode("{}")"#, encoded))
        .eval()
        .unwrap();
    assert_eq!(decoded, "test");
}
