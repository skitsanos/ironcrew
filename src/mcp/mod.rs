//! MCP (Model Context Protocol) client support for IronCrew.
//!
//! Gated by the `mcp` Cargo feature (on by default).
//!
//! ## Quick start
//!
//! In your `crew.lua`:
//! ```lua
//! local crew = Crew.new({
//!     goal = "...",
//!     mcp_servers = {
//!         local = {
//!             transport = "stdio",
//!             command   = "python3",
//!             args      = {"examples/mcp/stdio-tools/server.py"},
//!         },
//!     },
//! })
//! ```
//!
//! MCP tools are available under `mcp__<server>__<tool>` in agents' `tools` list.

pub mod bridge;
mod call;
pub mod client;
pub mod config;
mod execution_policy;
mod lifecycle;
pub mod manager;

pub use config::{McpConfig, parse_mcp_config};
pub use manager::McpConnectionManager;
