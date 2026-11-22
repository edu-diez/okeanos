//! Tool system.
//!
//! Provides the `Tool` trait, `ToolRegistry`, built-in tools, and result caching.

pub mod builtin;
pub mod cache;
pub mod todo;
pub mod tool_trait;

#[cfg(feature = "mcp")]
pub mod mcp;

pub use builtin::{DeleteTool, GrepTool, MAX_OUTPUT_BYTES, ReadTool, RunTool, WriteTool};
pub use cache::{CacheConfig, ToolResultCache};
pub use todo::{TodoArgs, TodoItem, TodoList, TodoTool};
pub use tool_trait::{Tool, ToolError, ToolExecutionResult, ToolRegistry, ToolSpec};

#[cfg(feature = "mcp")]
pub use mcp::{McpClient, McpClientConfig, McpError, McpServerConfig, McpTool};
