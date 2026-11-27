//! MCP (Model Context Protocol) client integration.
//!
//! Provides lazy, deferred MCP server handling behind the `mcp` feature flag.
//! - MCP client with stdio and Streamable HTTP transports
//! - Tool discovery from MCP servers at runtime
//! - Register discovered tools in `ToolRegistry`

#[cfg(feature = "mcp")]
pub mod client;

#[cfg(feature = "mcp")]
pub mod oauth;

#[cfg(feature = "mcp")]
pub mod presets;

#[cfg(feature = "mcp")]
pub mod project_config;

#[cfg(feature = "mcp")]
pub mod tool;

#[cfg(feature = "mcp")]
pub use project_config::{load_project_mcp_json, parse_mcp_json};

#[cfg(feature = "mcp")]
pub use client::{
    DiscoveredTool, McpClient, McpClientConfig, McpError, McpServerConfig, McpStartupPolicy,
    prefixed_tool_name,
};

#[cfg(feature = "mcp")]
pub use tool::{McpTool, register_mcp_tools};
