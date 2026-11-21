//! MCP (Model Context Protocol) client integration.
//!
//! Provides lazy MCP server handling behind the `mcp` feature flag.
//! - MCP client with stdio transport
//! - Tool discovery from MCP servers at runtime
//! - Register discovered tools in `ToolRegistry`

#[cfg(feature = "mcp")]
pub mod client;

#[cfg(feature = "mcp")]
pub mod tool;

#[cfg(feature = "mcp")]
pub use client::{McpClient, McpClientConfig, McpError, McpServerConfig, McpStartupPolicy};

#[cfg(feature = "mcp")]
pub use tool::McpTool;
