//! Tool system.
//!
//! Provides the `Tool` trait, `ToolRegistry`, and built-in tools.

pub mod todo;
pub mod tool_trait;

pub use todo::{TodoArgs, TodoItem, TodoList, TodoTool};
pub use tool_trait::{Tool, ToolError, ToolRegistry, ToolSpec};
