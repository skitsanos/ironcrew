pub mod chat;
pub mod commands;
pub mod graph;
pub mod graph_bundle;
pub mod graph_extract;
pub mod graph_types;
pub mod history;
pub mod project;
pub mod server;

/// Escape a string for safe embedding as a Lua literal. Used by the chat
/// CLI and the HTTP conversation start handler to drive `crew:conversation`
/// without duplicating the Lua-side option parsing logic.
pub fn chat_lua_literal(s: &str) -> String {
    chat::__chat_lua_literal(s)
}
