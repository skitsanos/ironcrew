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
//!         git = {
//!             transport = "stdio",
//!             command   = "uvx",
//!             args      = {"mcp-server-git"},
//!         },
//!     },
//! })
//! ```
//!
//! MCP tools are available under `mcp__<server>__<tool>` in agents' `tools` list.

pub mod bridge;
pub mod client;
pub mod config;
pub mod manager;

pub use config::{McpConfig, parse_mcp_config};
pub use manager::McpConnectionManager;
