use std::collections::HashSet;
use std::io::{self, Write};

use mlua::{Lua, Result as LuaResult, Table, Value};

const DEFAULT_MAX_DEPTH: usize = 64;
const DEFAULT_MAX_NODES: usize = 100_000;
const DEFAULT_MAX_STRING_BYTES: usize = 8 * 1024 * 1024;
const DEFAULT_MAX_OUTPUT_BYTES: usize = 16 * 1024 * 1024;

const MAX_DEPTH_CEILING: usize = 256;
const MAX_NODES_CEILING: usize = 1_000_000;
const MAX_STRING_BYTES_CEILING: usize = 256 * 1024 * 1024;
const MAX_OUTPUT_BYTES_CEILING: usize = 256 * 1024 * 1024;

#[derive(Clone, Copy, Debug)]
struct JsonLimits {
    max_depth: usize,
    max_nodes: usize,
    max_string_bytes: usize,
    max_output_bytes: usize,
}

impl JsonLimits {
    fn from_env() -> LuaResult<Self> {
        Ok(Self {
            max_depth: bounded_env(
                "IRONCREW_LUA_JSON_MAX_DEPTH",
                DEFAULT_MAX_DEPTH,
                1,
                MAX_DEPTH_CEILING,
            )?,
            max_nodes: bounded_env(
                "IRONCREW_LUA_JSON_MAX_NODES",
                DEFAULT_MAX_NODES,
                1,
                MAX_NODES_CEILING,
            )?,
            max_string_bytes: bounded_env(
                "IRONCREW_LUA_JSON_MAX_STRING_BYTES",
                DEFAULT_MAX_STRING_BYTES,
                1,
                MAX_STRING_BYTES_CEILING,
            )?,
            max_output_bytes: bounded_env(
                "IRONCREW_LUA_JSON_MAX_OUTPUT_BYTES",
                DEFAULT_MAX_OUTPUT_BYTES,
                1,
                MAX_OUTPUT_BYTES_CEILING,
            )?,
        })
    }
}

fn bounded_env(name: &str, default: usize, min: usize, max: usize) -> LuaResult<usize> {
    let raw = match std::env::var(name) {
        Ok(raw) => raw,
        Err(std::env::VarError::NotPresent) => return Ok(default),
        Err(std::env::VarError::NotUnicode(_)) => {
            return Err(mlua::Error::external(format!(
                "{name} must contain valid Unicode digits"
            )));
        }
    };
    let value = raw.parse::<usize>().map_err(|_| {
        mlua::Error::external(format!(
            "{name} must be a whole number between {min} and {max}"
        ))
    })?;
    if !(min..=max).contains(&value) {
        return Err(mlua::Error::external(format!(
            "{name} must be between {min} and {max}; got {value}"
        )));
    }
    Ok(value)
}

#[derive(Debug)]
struct ConversionState {
    limits: JsonLimits,
    nodes: usize,
    string_bytes: usize,
    active_tables: HashSet<usize>,
}

impl ConversionState {
    fn new(limits: JsonLimits) -> Self {
        Self {
            limits,
            nodes: 0,
            string_bytes: 0,
            active_tables: HashSet::new(),
        }
    }

    fn visit_node(&mut self, depth: usize) -> LuaResult<()> {
        if depth > self.limits.max_depth {
            return Err(mlua::Error::external(format!(
                "Lua/JSON conversion exceeded maximum depth of {}",
                self.limits.max_depth
            )));
        }
        self.nodes = self.nodes.saturating_add(1);
        if self.nodes > self.limits.max_nodes {
            return Err(mlua::Error::external(format!(
                "Lua/JSON conversion exceeded maximum node count of {}",
                self.limits.max_nodes
            )));
        }
        Ok(())
    }

    fn add_string(&mut self, value: &str) -> LuaResult<()> {
        self.string_bytes = self.string_bytes.saturating_add(value.len());
        if self.string_bytes > self.limits.max_string_bytes {
            return Err(mlua::Error::external(format!(
                "Lua/JSON conversion exceeded aggregate string limit of {} bytes",
                self.limits.max_string_bytes
            )));
        }
        Ok(())
    }

    fn ensure_table_entries_fit(&self, count: usize) -> LuaResult<()> {
        if count > self.limits.max_nodes.saturating_sub(self.nodes) {
            return Err(mlua::Error::external(format!(
                "Lua/JSON conversion exceeded maximum node count of {}",
                self.limits.max_nodes
            )));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TableShape {
    Array(usize),
    Object,
}

fn classify_table(table: &Table, state: &ConversionState) -> LuaResult<TableShape> {
    let mut entries = 0usize;
    let mut integer_keys = 0usize;
    let mut string_keys = 0usize;
    let mut max_index = 0usize;

    for pair in table.clone().pairs::<Value, Value>() {
        let (key, _) = pair?;
        entries = entries.saturating_add(1);
        state.ensure_table_entries_fit(entries)?;

        match key {
            Value::Integer(index) => {
                let index = usize::try_from(index).map_err(|_| {
                    mlua::Error::external(
                        "Lua arrays must use positive, contiguous integer keys starting at 1",
                    )
                })?;
                if index == 0 {
                    return Err(mlua::Error::external(
                        "Lua arrays must use positive, contiguous integer keys starting at 1",
                    ));
                }
                integer_keys += 1;
                max_index = max_index.max(index);
            }
            Value::String(_) => string_keys += 1,
            other => {
                return Err(mlua::Error::external(format!(
                    "Lua table key type '{}' cannot be represented in JSON",
                    other.type_name()
                )));
            }
        }
    }

    if entries == 0 {
        return Ok(TableShape::Object);
    }
    if integer_keys > 0 && string_keys > 0 {
        return Err(mlua::Error::external(
            "Lua tables with mixed integer and string keys cannot be represented in JSON",
        ));
    }
    if integer_keys > 0 {
        if max_index != entries {
            return Err(mlua::Error::external(
                "Sparse Lua arrays cannot be represented in JSON",
            ));
        }
        return Ok(TableShape::Array(entries));
    }
    Ok(TableShape::Object)
}

fn table_to_json(
    table: &Table,
    state: &mut ConversionState,
    depth: usize,
) -> LuaResult<serde_json::Value> {
    let identity = table.to_pointer() as usize;
    if !state.active_tables.insert(identity) {
        return Err(mlua::Error::external(
            "Cycle detected while converting a Lua table to JSON",
        ));
    }

    let result = (|| {
        let shape = classify_table(table, state)?;
        match shape {
            TableShape::Array(len) => {
                let mut values = Vec::with_capacity(len);
                for index in 1..=len {
                    let value: Value = table.raw_get(index)?;
                    values.push(value_to_json(value, state, depth + 1)?);
                }
                Ok(serde_json::Value::Array(values))
            }
            TableShape::Object => {
                let mut values = serde_json::Map::new();
                for pair in table.clone().pairs::<Value, Value>() {
                    let (key, value) = pair?;
                    let Value::String(key) = key else {
                        return Err(mlua::Error::external(
                            "Lua JSON objects must use string keys",
                        ));
                    };
                    let key = key.to_str()?;
                    state.add_string(&key)?;
                    values.insert(key.to_string(), value_to_json(value, state, depth + 1)?);
                }
                Ok(serde_json::Value::Object(values))
            }
        }
    })();

    state.active_tables.remove(&identity);
    result
}

fn value_to_json(
    value: Value,
    state: &mut ConversionState,
    depth: usize,
) -> LuaResult<serde_json::Value> {
    state.visit_node(depth)?;
    match value {
        Value::Nil => Ok(serde_json::Value::Null),
        Value::Boolean(value) => Ok(serde_json::Value::Bool(value)),
        Value::Integer(value) => Ok(serde_json::Value::Number(value.into())),
        Value::Number(value) => {
            if !value.is_finite() {
                return Err(mlua::Error::external(
                    "Non-finite Lua numbers cannot be represented in JSON",
                ));
            }
            serde_json::Number::from_f64(value)
                .map(serde_json::Value::Number)
                .ok_or_else(|| {
                    mlua::Error::external("Lua number cannot be represented safely in JSON")
                })
        }
        Value::String(value) => {
            let value = value.to_str()?;
            state.add_string(&value)?;
            Ok(serde_json::Value::String(value.to_string()))
        }
        Value::Table(table) => table_to_json(&table, state, depth),
        other => Err(mlua::Error::external(format!(
            "Lua value type '{}' cannot be represented in JSON",
            other.type_name()
        ))),
    }
}

#[derive(Debug)]
struct LimitedWriter {
    written: usize,
    limit: usize,
    exceeded: bool,
}

impl Write for LimitedWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.written.saturating_add(bytes.len()) > self.limit {
            self.exceeded = true;
            return Err(io::Error::other("JSON output limit exceeded"));
        }
        self.written += bytes.len();
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn ensure_output_fits(value: &serde_json::Value, max_bytes: usize) -> LuaResult<()> {
    let mut writer = LimitedWriter {
        written: 0,
        limit: max_bytes,
        exceeded: false,
    };
    if let Err(error) = serde_json::to_writer(&mut writer, value) {
        if writer.exceeded {
            return Err(mlua::Error::external(format!(
                "Lua/JSON conversion exceeded serialized output limit of {max_bytes} bytes"
            )));
        }
        return Err(mlua::Error::external(format!(
            "Failed to measure serialized JSON output: {error}"
        )));
    }
    Ok(())
}

/// Convert a Lua table to a bounded JSON value.
pub fn lua_table_to_json(table: &Table) -> LuaResult<serde_json::Value> {
    lua_value_to_json(Value::Table(table.clone()))
}

/// Convert a Lua value to JSON without silently coercing unsupported values.
pub fn lua_value_to_json(value: Value) -> LuaResult<serde_json::Value> {
    let limits = JsonLimits::from_env()?;
    lua_value_to_json_with_limits(value, limits)
}

fn lua_value_to_json_with_limits(value: Value, limits: JsonLimits) -> LuaResult<serde_json::Value> {
    let mut state = ConversionState::new(limits);
    let value = value_to_json(value, &mut state, 0)?;
    ensure_output_fits(&value, limits.max_output_bytes)?;
    Ok(value)
}

fn validate_json_value(
    value: &serde_json::Value,
    state: &mut ConversionState,
    depth: usize,
) -> LuaResult<()> {
    state.visit_node(depth)?;
    match value {
        serde_json::Value::String(value) => state.add_string(value),
        serde_json::Value::Array(values) => {
            state.ensure_table_entries_fit(values.len())?;
            for value in values {
                validate_json_value(value, state, depth + 1)?;
            }
            Ok(())
        }
        serde_json::Value::Object(values) => {
            state.ensure_table_entries_fit(values.len())?;
            for (key, value) in values {
                state.add_string(key)?;
                validate_json_value(value, state, depth + 1)?;
            }
            Ok(())
        }
        serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => {
            Ok(())
        }
    }
}

fn json_to_lua_unchecked(lua: &Lua, value: &serde_json::Value) -> LuaResult<Value> {
    match value {
        serde_json::Value::Null => Ok(Value::Nil),
        serde_json::Value::Bool(value) => Ok(Value::Boolean(*value)),
        serde_json::Value::Number(value) => {
            if let Some(integer) = value.as_i64() {
                Ok(Value::Integer(integer))
            } else if value.as_u64().is_some() {
                Err(mlua::Error::external(
                    "JSON integer is outside Lua's signed 64-bit integer range",
                ))
            } else if let Some(number) = value.as_f64() {
                if !number.is_finite() {
                    return Err(mlua::Error::external(
                        "Non-finite JSON numbers cannot be represented in Lua",
                    ));
                }
                Ok(Value::Number(number))
            } else {
                Err(mlua::Error::external(
                    "JSON number cannot be represented safely in Lua",
                ))
            }
        }
        serde_json::Value::String(value) => Ok(Value::String(lua.create_string(value)?)),
        serde_json::Value::Array(values) => {
            let table = lua.create_table_with_capacity(values.len(), 0)?;
            for (index, value) in values.iter().enumerate() {
                table.raw_set(index + 1, json_to_lua_unchecked(lua, value)?)?;
            }
            Ok(Value::Table(table))
        }
        serde_json::Value::Object(values) => {
            let table = lua.create_table_with_capacity(0, values.len())?;
            for (key, value) in values {
                table.raw_set(key.as_str(), json_to_lua_unchecked(lua, value)?)?;
            }
            Ok(Value::Table(table))
        }
    }
}

/// Convert a JSON value into Lua after validating its complete resource cost.
pub fn json_value_to_lua(lua: &Lua, value: &serde_json::Value) -> LuaResult<Value> {
    let limits = JsonLimits::from_env()?;
    let mut state = ConversionState::new(limits);
    validate_json_value(value, &mut state, 0)?;
    ensure_output_fits(value, limits.max_output_bytes)?;
    json_to_lua_unchecked(lua, value)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits() -> JsonLimits {
        JsonLimits {
            max_depth: 4,
            max_nodes: 16,
            max_string_bytes: 32,
            max_output_bytes: 64,
        }
    }

    #[test]
    fn rejects_self_referential_table() {
        let lua = Lua::new();
        let table = lua.create_table().unwrap();
        table.set("self", table.clone()).unwrap();

        let error = lua_value_to_json_with_limits(Value::Table(table), limits()).unwrap_err();
        assert!(error.to_string().contains("Cycle detected"));
    }

    #[test]
    fn repeated_non_cyclic_table_is_allowed() {
        let lua = Lua::new();
        let child = lua.create_table().unwrap();
        child.set("value", 1).unwrap();
        let root = lua.create_table().unwrap();
        root.set("left", child.clone()).unwrap();
        root.set("right", child).unwrap();

        let converted = lua_value_to_json_with_limits(Value::Table(root), limits()).unwrap();
        assert_eq!(converted["left"]["value"], 1);
        assert_eq!(converted["right"]["value"], 1);
    }

    #[test]
    fn rejects_non_finite_numbers_and_functions() {
        let lua = Lua::new();
        let error = lua_value_to_json_with_limits(Value::Number(f64::NAN), limits()).unwrap_err();
        assert!(error.to_string().contains("Non-finite"));

        let function = lua.create_function(|_, ()| Ok(())).unwrap();
        let error = lua_value_to_json_with_limits(Value::Function(function), limits()).unwrap_err();
        assert!(error.to_string().contains("cannot be represented"));
    }

    #[test]
    fn rejects_sparse_mixed_and_invalid_key_tables() {
        let lua = Lua::new();

        let sparse = lua.create_table().unwrap();
        sparse.set(2, "value").unwrap();
        let error = lua_value_to_json_with_limits(Value::Table(sparse), limits()).unwrap_err();
        assert!(error.to_string().contains("Sparse"));

        let mixed = lua.create_table().unwrap();
        mixed.set(1, "value").unwrap();
        mixed.set("name", "value").unwrap();
        let error = lua_value_to_json_with_limits(Value::Table(mixed), limits()).unwrap_err();
        assert!(error.to_string().contains("mixed"));

        let invalid = lua.create_table().unwrap();
        invalid.set(true, "value").unwrap();
        let error = lua_value_to_json_with_limits(Value::Table(invalid), limits()).unwrap_err();
        assert!(error.to_string().contains("key type"));
    }

    #[test]
    fn enforces_depth_node_string_and_serialized_output_limits() {
        let lua = Lua::new();

        let mut deep = lua.create_table().unwrap();
        let root = deep.clone();
        for _ in 0..5 {
            let child = lua.create_table().unwrap();
            deep.set("child", child.clone()).unwrap();
            deep = child;
        }
        let error = lua_value_to_json_with_limits(Value::Table(root), limits()).unwrap_err();
        assert!(error.to_string().contains("depth"));

        let many = lua.create_table().unwrap();
        for index in 1..=17 {
            many.set(index, index).unwrap();
        }
        let error = lua_value_to_json_with_limits(Value::Table(many), limits()).unwrap_err();
        assert!(error.to_string().contains("node count"));

        let long = Value::String(lua.create_string("x".repeat(33)).unwrap());
        let error = lua_value_to_json_with_limits(long, limits()).unwrap_err();
        assert!(error.to_string().contains("aggregate string"));

        let escaping = Value::String(lua.create_string("\n".repeat(32)).unwrap());
        let error = lua_value_to_json_with_limits(escaping, limits()).unwrap_err();
        assert!(error.to_string().contains("serialized output"));
    }

    #[test]
    fn json_to_lua_validates_before_allocating() {
        let lua = Lua::new();
        let value = serde_json::json!({"value": "x".repeat(33)});
        let mut state = ConversionState::new(limits());
        let error = validate_json_value(&value, &mut state, 0).unwrap_err();
        assert!(error.to_string().contains("aggregate string"));

        let valid = serde_json::json!({"items": [1, 2, 3]});
        let converted = json_value_to_lua(&lua, &valid).unwrap();
        assert!(matches!(converted, Value::Table(_)));
    }
}
