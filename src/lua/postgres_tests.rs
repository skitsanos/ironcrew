use super::*;
use mlua::Value;

#[test]
fn stub_calls_fail_with_the_configuration_hint() {
    let lua = Lua::new();
    register_postgres_stub(&lua, STUB_UNCONFIGURED).unwrap();
    let error = lua
        .load("return postgres.query('anything')")
        .eval::<Value>()
        .unwrap_err()
        .to_string();
    assert!(error.contains("IRONCREW_APP_DATABASE_URL"), "{error}");
}

#[test]
fn tool_vm_has_no_postgres_namespace() {
    let lua = crate::lua::sandbox::create_tool_lua().unwrap();
    let value: Value = lua.globals().get("postgres").unwrap();
    assert!(
        matches!(value, Value::Nil),
        "tool sandbox must not see postgres.*"
    );
}

#[cfg(feature = "postgres")]
mod params_for_validation {
    use super::super::params_for;
    use crate::engine::app_db::{AppDb, operations::OperationRegistry, policy::AppDbPolicy};
    use mlua::{Lua, Table};

    fn fixture(max_param_bytes: u64) -> AppDb {
        let policy =
            AppDbPolicy::from_values(4, 5_000, 500, 1 << 20, max_param_bytes, 64, 64 * 1024);
        let sources = vec![
            (
                "save".to_string(),
                "-- ironcrew:op\n-- params: run_id text, payload json\nSELECT $1, $2;\n"
                    .to_string(),
            ),
            (
                "count".to_string(),
                "-- ironcrew:op\nSELECT 1;\n".to_string(),
            ),
        ];
        let registry = OperationRegistry::from_sources(sources, &policy).unwrap();
        AppDb::new("postgres://invalid.invalid/x".into(), policy, registry)
    }

    fn lua_table(lua: &Lua, entries: &[(&str, &str)]) -> Table {
        let table = lua.create_table().unwrap();
        for (key, value) in entries {
            table.set(*key, *value).unwrap();
        }
        table
    }

    #[test]
    fn unknown_param_error_lists_declared_params() {
        let lua = Lua::new();
        let app = fixture(1 << 20);
        let table = lua_table(&lua, &[("run_id", "x"), ("bogus", "y")]);
        let error = params_for(&app, "save", Some(table))
            .unwrap_err()
            .to_string();
        assert!(error.contains("'bogus'"), "{error}");
        assert!(error.contains("run_id, payload"), "{error}");
    }

    #[test]
    fn zero_param_op_unknown_key_says_none() {
        let lua = Lua::new();
        let app = fixture(1 << 20);
        let table = lua_table(&lua, &[("stray", "y")]);
        let error = params_for(&app, "count", Some(table))
            .unwrap_err()
            .to_string();
        assert!(error.contains("Declared params: none"), "{error}");
    }

    #[test]
    fn non_string_keys_are_rejected() {
        let lua = Lua::new();
        let app = fixture(1 << 20);
        let table = lua.create_table().unwrap();
        table.set(1, "positional").unwrap();
        let error = params_for(&app, "save", Some(table))
            .unwrap_err()
            .to_string();
        assert!(error.contains("string keys"), "{error}");
    }

    #[test]
    fn omitted_declared_param_is_rejected() {
        let lua = Lua::new();
        let app = fixture(1 << 20);
        let table = lua_table(&lua, &[("run_id", "x")]);
        let error = params_for(&app, "save", Some(table))
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("missing declared param 'payload'"),
            "{error}"
        );
    }

    #[test]
    fn per_param_byte_budget_is_enforced() {
        let lua = Lua::new();
        let app = fixture(8);
        let table = lua_table(
            &lua,
            &[("run_id", "this string serializes past eight bytes")],
        );
        let error = params_for(&app, "save", Some(table))
            .unwrap_err()
            .to_string();
        assert!(error.contains("IRONCREW_APP_DB_MAX_PARAM_BYTES"), "{error}");
    }

    #[tokio::test]
    async fn http_bootstrap_rejects_every_postgres_effect_before_connecting() {
        let lua = Lua::new();
        let app = std::sync::Arc::new(fixture(1 << 20));
        super::super::register_postgres(&lua, app).unwrap();
        lua.set_app_data(crate::lua::bootstrap::HttpConversationBootstrap);

        for call in [
            "postgres.execute('count')",
            "postgres.query('count')",
            "postgres.query_one('count')",
        ] {
            let error = lua
                .load(format!("return {call}"))
                .eval_async::<mlua::Value>()
                .await
                .unwrap_err()
                .to_string();
            assert!(error.contains("HTTP conversation bootstrap"), "{error}");
        }
    }
}
