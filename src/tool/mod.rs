//! Tool system.
//!
//! Provides the `Tool` trait, `ToolRegistry`, built-in tools, and result caching.

pub mod agent;
pub mod builtin;
pub mod cache;
pub mod run_env;
pub mod sandbox;
pub mod tier;
pub mod todo;
pub mod tool_trait;
pub mod workspace;

#[cfg(feature = "mcp")]
pub mod mcp;

pub use builtin::{DeleteTool, GrepTool, MAX_OUTPUT_BYTES, ReadTool, RunTool, WriteTool};
pub use cache::{CacheConfig, ToolResultCache};
pub use tier::builtin_registry;
pub use todo::{TodoArgs, TodoItem, TodoList, TodoTool};
pub use tool_trait::{
    Tool, ToolError, ToolExecutionResult, ToolOutputSink, ToolRegistry, ToolSpec,
};

#[cfg(feature = "mcp")]
pub use mcp::{McpClient, McpClientConfig, McpError, McpServerConfig, McpTool};
