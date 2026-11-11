//! Tool system.
//!
//! Provides the `Tool` trait, `ToolRegistry`, and built-in tools.

pub mod builtin;
pub mod todo;
pub mod tool_trait;

pub use builtin::{DeleteTool, GrepTool, MAX_OUTPUT_BYTES, ReadTool, RunTool, WriteTool};
pub use todo::{TodoArgs, TodoItem, TodoList, TodoTool};
pub use tool_trait::{Tool, ToolError, ToolExecutionResult, ToolRegistry, ToolSpec};
