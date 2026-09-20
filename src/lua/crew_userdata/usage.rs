use super::LuaCrew;
use crate::lua::usage;
use mlua::{UserDataMethods, Value};

pub(super) fn register<M: UserDataMethods<LuaCrew>>(methods: &mut M) {
    methods.add_method("usage", |lua, this, ()| {
        let tracker = this
            .last_run_usage
            .lock()
            .expect("usage lock poisoned")
            .clone();
        match tracker {
            Some(tracker) => usage::snapshot(lua, &tracker),
            None => Ok(Value::Nil),
        }
    });
    methods.add_method("flow_usage", |lua, _, ()| {
        usage::snapshot(lua, &usage::tracker(lua))
    });
}
