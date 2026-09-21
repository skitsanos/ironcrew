//! `postgres.*` — flow-facing named-operation access to the app database.
//! Crew sandbox only; the tool VM never receives this namespace.

use mlua::{Lua, Result as LuaResult};

// Each hint string is referenced from exactly one arm of the `#[cfg(feature =
// "postgres")]` / `#[cfg(not(...))]` split in `setup_crew_runtime_inner`, so
// it is genuinely unused under the *other* feature configuration's plain
// `cargo build` (the unconditional unit test below covers STUB_UNCONFIGURED
// under both configurations).
#[cfg_attr(not(feature = "postgres"), allow(dead_code))]
pub const STUB_UNCONFIGURED: &str =
    "postgres.* is not configured: set IRONCREW_APP_DATABASE_URL (see docs/postgres-app-data.md)";
#[cfg_attr(feature = "postgres", allow(dead_code))]
pub const STUB_NO_FEATURE: &str =
    "postgres.* is unavailable: this binary was built without the 'postgres' cargo feature";
pub const STUB_SUBFLOW: &str = "postgres.* is not available inside run_flow sub-flows in this version; perform app-database operations in the parent flow and pass results in via input";

/// Register a namespace whose every call fails with `reason`. Fail-closed but
/// diagnosable: a flow calling postgres.* gets a configuration hint instead of
/// a nil-index error.
pub fn register_postgres_stub(lua: &Lua, reason: &'static str) -> LuaResult<()> {
    let table = lua.create_table()?;
    for method in ["execute", "query", "query_one"] {
        let capability = match method {
            "execute" => "postgres.execute",
            "query" => "postgres.query",
            _ => "postgres.query_one",
        };
        table.set(
            method,
            lua.create_function(move |lua, _: mlua::MultiValue| -> LuaResult<()> {
                crate::lua::bootstrap::reject_effect(lua, capability)?;
                Err(mlua::Error::external(reason))
            })?,
        )?;
    }
    lua.globals().set("postgres", table)
}

/// Convert a Lua params table into ordered JSON bind values for one declared
/// operation. Module-level (rather than nested in `register_postgres`) so the
/// validation paths are unit-testable without a database.
#[cfg(feature = "postgres")]
fn params_for(
    app_db: &crate::engine::app_db::AppDb,
    name: &str,
    table: Option<mlua::Table>,
) -> LuaResult<Vec<serde_json::Value>> {
    use crate::lua::json::lua_value_to_json;
    use mlua::Value;

    let operation = app_db.operation(name).map_err(mlua::Error::external)?;
    let max_param_bytes = app_db.policy().max_param_bytes();
    let table = match table {
        Some(table) => table,
        None if operation.params.is_empty() => return Ok(Vec::new()),
        None => {
            return Err(mlua::Error::external(format!(
                "postgres operation '{name}' expects a params table"
            )));
        }
    };
    // Reject unknown keys (IC-028 house rule).
    for pair in table.clone().pairs::<Value, Value>() {
        let (key, _) = pair?;
        let Value::String(key) = key else {
            return Err(mlua::Error::external(format!(
                "postgres operation '{name}': params table must use string keys"
            )));
        };
        let key = key.to_str()?.to_string();
        if !operation.params.iter().any(|(param, _)| *param == key) {
            let supported: Vec<&str> = operation
                .params
                .iter()
                .map(|(param, _)| param.as_str())
                .collect();
            let supported = if supported.is_empty() {
                "none".to_string()
            } else {
                supported.join(", ")
            };
            return Err(mlua::Error::external(format!(
                "postgres operation '{name}': unknown param '{key}'. Declared params: {supported}"
            )));
        }
    }
    let mut values = Vec::with_capacity(operation.params.len());
    for (param, _) in &operation.params {
        if !table.contains_key(param.as_str())? {
            return Err(mlua::Error::external(format!(
                "postgres operation '{name}': missing declared param '{param}'"
            )));
        }
        let value: Value = table.get(param.as_str())?;
        let json = lua_value_to_json(value)
            .map_err(|e| mlua::Error::external(format!("param '{param}': {e}")))?;
        let bytes = serde_json::to_string(&json)
            .map(|s| s.len())
            .unwrap_or(usize::MAX);
        if bytes > max_param_bytes {
            return Err(mlua::Error::external(format!(
                "param '{param}' exceeds IRONCREW_APP_DB_MAX_PARAM_BYTES ({max_param_bytes})"
            )));
        }
        values.push(json);
    }
    Ok(values)
}

#[cfg(feature = "postgres")]
pub fn register_postgres(
    lua: &Lua,
    app_db: std::sync::Arc<crate::engine::app_db::AppDb>,
) -> LuaResult<()> {
    use crate::lua::json::json_value_to_lua;
    use mlua::{Table, Value};

    let table = lua.create_table()?;

    let db = app_db.clone();
    table.set(
        "execute",
        lua.create_async_function(move |lua, (name, params): (String, Option<Table>)| {
            let db = db.clone();
            async move {
                crate::lua::bootstrap::reject_effect(&lua, "postgres.execute")?;
                let values = params_for(&db, &name, params)?;
                let affected = db
                    .execute(&name, &values)
                    .await
                    .map_err(mlua::Error::external)?;
                // Saturate rather than wrap: rows_affected > i64::MAX is not
                // reachable in practice, but a wrapped negative count would be
                // a lie to the flow.
                Ok(i64::try_from(affected).unwrap_or(i64::MAX))
            }
        })?,
    )?;

    let db = app_db.clone();
    table.set(
        "query",
        lua.create_async_function(move |lua, (name, params): (String, Option<Table>)| {
            let db = db.clone();
            async move {
                crate::lua::bootstrap::reject_effect(&lua, "postgres.query")?;
                let values = params_for(&db, &name, params)?;
                let rows = db
                    .query(&name, &values)
                    .await
                    .map_err(mlua::Error::external)?;
                json_value_to_lua(&lua, &serde_json::Value::Array(rows))
            }
        })?,
    )?;

    let db = app_db;
    table.set(
        "query_one",
        lua.create_async_function(move |lua, (name, params): (String, Option<Table>)| {
            let db = db.clone();
            async move {
                crate::lua::bootstrap::reject_effect(&lua, "postgres.query_one")?;
                let values = params_for(&db, &name, params)?;
                match db
                    .query_one(&name, &values)
                    .await
                    .map_err(mlua::Error::external)?
                {
                    Some(row) => json_value_to_lua(&lua, &row),
                    None => Ok(Value::Nil),
                }
            }
        })?,
    )?;

    lua.globals().set("postgres", table)
}

#[cfg(test)]
#[path = "postgres_tests.rs"]
mod tests;
