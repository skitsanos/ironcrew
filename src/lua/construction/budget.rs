//! Bound host-retained declarations too: the Lua heap limit alone does not
//! cover Rust strings cloned from a table repeatedly in a loop.
use mlua::{Lua, Table, Value};

pub(crate) fn admit(lua: &Lua, table: &Table) -> mlua::Result<()> {
    let Some(evaluation) = lua.app_data_ref::<super::Evaluation>() else {
        return Ok(());
    };
    let mut state = evaluation.0.lock().unwrap();
    state.declarations += 1;
    if state.declarations > 256 {
        return Err(mlua::Error::external(
            "construction exceeds 256 declarations",
        ));
    }
    let mut pending = vec![(table.clone(), 0)];
    while let Some((table, depth)) = pending.pop() {
        if depth > 32 {
            return Err(mlua::Error::external("construction table exceeds depth 32"));
        }
        for pair in table.pairs::<Value, Value>() {
            let (key, value) = pair?;
            state.nodes += 1;
            if state.nodes > 16_384 {
                return Err(mlua::Error::external(
                    "construction exceeds 16384 table entries",
                ));
            }
            for value in [key, value] {
                match value {
                    Value::Table(table) => pending.push((table, depth + 1)),
                    Value::String(string) => {
                        state.bytes = state.bytes.saturating_add(string.as_bytes().len())
                    }
                    Value::Function(function) => {
                        state.bytes = state.bytes.saturating_add(function.dump(false).len())
                    }
                    _ => {}
                }
            }
            if state.bytes > 8 * 1024 * 1024 {
                return Err(mlua::Error::external(
                    "construction exceeds 8 MiB of declaration strings",
                ));
            }
        }
    }
    Ok(())
}

/// Break any host-owned Lua handles on success, error, or future cancellation.
pub(super) struct Cleanup(pub super::Evaluation);

impl Drop for Cleanup {
    fn drop(&mut self) {
        self.0.0.lock().unwrap().crews.clear();
    }
}
