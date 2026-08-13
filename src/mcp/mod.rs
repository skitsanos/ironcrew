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
mod client_connect;
mod client_operations;
pub mod config;
mod connection;
mod execution_policy;
#[cfg(test)]
mod execution_policy_tests;
mod http_body;
mod http_headers;
mod http_tool_headers;
mod http_tool_schema;
mod http_transport;
mod lifecycle;
pub mod manager;
mod protocol;
mod sse_stream;
mod stdio_transport;

pub use config::{McpConfig, parse_mcp_config};
pub use manager::McpConnectionManager;
