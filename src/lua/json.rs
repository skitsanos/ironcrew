use mlua::{Lua, Result as LuaResult, Table, Value};

/// Recursively convert a Lua table to serde_json::Value.
pub fn lua_table_to_json(table: &Table) -> LuaResult<serde_json::Value> {
    // Check if it's an array (sequential integer keys starting at 1)
    let is_array = table.clone().sequence_values::<Value>().next().is_some()
        && table.clone().pairs::<Value, Value>().all(|pair| {
            pair.map(|(k, _)| matches!(k, Value::Integer(_)))
                .unwrap_or(false)
        });

    if is_array {
        let arr: Vec<serde_json::Value> = table
            .clone()
            .sequence_values::<Value>()
            .map(|v| lua_value_to_json(v.unwrap_or(Value::Nil)))
            .collect::<LuaResult<Vec<_>>>()?;
        Ok(serde_json::Value::Array(arr))
    } else {
        let mut map = serde_json::Map::new();
        for pair in table.clone().pairs::<String, Value>() {
            let (key, value) = pair?;
            map.insert(key, lua_value_to_json(value)?);
        }
        Ok(serde_json::Value::Object(map))
    }
}

pub fn lua_value_to_json(value: Value) -> LuaResult<serde_json::Value> {
    match value {
        Value::Nil => Ok(serde_json::Value::Null),
        Value::Boolean(b) => Ok(serde_json::Value::Bool(b)),
        Value::Integer(i) => Ok(serde_json::json!(i)),
        Value::Number(n) => Ok(serde_json::json!(n)),
        Value::String(s) => Ok(serde_json::Value::String(s.to_str()?.to_string())),
        Value::Table(t) => lua_table_to_json(&t),
        _ => Ok(serde_json::Value::Null),
    }
}

/// Convert a serde_json::Value into a Lua value.
pub fn json_value_to_lua(lua: &Lua, value: &serde_json::Value) -> LuaResult<Value> {
    match value {
        serde_json::Value::Null => Ok(Value::Nil),
        serde_json::Value::Bool(b) => Ok(Value::Boolean(*b)),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Ok(Value::Integer(i))
            } else if let Some(f) = n.as_f64() {
                Ok(Value::Number(f))
            } else {
                Ok(Value::Nil)
            }
        }
        serde_json::Value::String(s) => Ok(Value::String(lua.create_string(s)?)),
        serde_json::Value::Array(arr) => {
            let table = lua.create_table()?;
            for (i, v) in arr.iter().enumerate() {
                table.set(i + 1, json_value_to_lua(lua, v)?)?;
            }
            Ok(Value::Table(table))
        }
        serde_json::Value::Object(map) => {
            let table = lua.create_table()?;
            for (k, v) in map {
                table.set(k.as_str(), json_value_to_lua(lua, v)?)?;
            }
            Ok(Value::Table(table))
        }
    }
}
