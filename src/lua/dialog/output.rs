use super::{DialogTurn, speaker_label};
use mlua::Table;

pub(super) fn turn_to_lua(lua: &mlua::Lua, turn: &DialogTurn) -> mlua::Result<Table> {
    let table = lua.create_table()?;
    table.set("index", turn.index)?;
    table.set("speaker", speaker_label(turn.speaker_index))?;
    table.set("agent", turn.agent_name.clone())?;
    table.set("content", turn.content.clone())?;
    if let Some(ref r) = turn.reasoning {
        table.set("reasoning", r.clone())?;
    }
    Ok(table)
}

pub(super) fn transcript_to_lua(lua: &mlua::Lua, transcript: &[DialogTurn]) -> mlua::Result<Table> {
    let table = lua.create_table()?;
    for (i, turn) in transcript.iter().enumerate() {
        table.set(i + 1, turn_to_lua(lua, turn)?)?;
    }
    Ok(table)
}
