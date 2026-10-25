pub mod api;
pub mod cli;
pub mod engine;
// Inspection is a library-only surface; the CLI delegates instead of compiling
// a second private copy (including in feature-disabled executables).
#[path = "cli/legacy_history.rs"]
pub mod history_cli;
#[path = "engine/history_reader.rs"]
pub mod history_reader;
pub mod llm;
pub mod lua;
#[cfg(feature = "mcp")]
pub mod mcp;
pub mod metrics;
pub mod tools;
pub mod usage;
pub mod utils;
