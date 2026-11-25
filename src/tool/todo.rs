//! Built-in todo tool.

use crate::tool::tool_trait::{Tool, ToolError, ToolExecutionResult, ToolSpec};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::{Arc, Mutex};

/// Todo item.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TodoItem {
    pub id: String,
    pub title: String,
    pub done: bool,
}

/// Shared todo list state.
#[derive(Debug, Default, Clone)]
pub struct TodoList {
    items: Arc<Mutex<Vec<TodoItem>>>,
}

impl TodoList {
    /// Create a new empty todo list.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a todo item.
    pub fn add(&self, title: String) -> TodoItem {
        let mut items = self.items.lock().unwrap();
        let id = format!("todo-{}", items.len() + 1);
        let item = TodoItem {
            id: id.clone(),
            title,
            done: false,
        };
        items.push(item.clone());
        item
    }

    /// Complete a todo item by ID.
    pub fn complete(&self, id: &str) -> Result<TodoItem, ToolError> {
        let mut items = self.items.lock().unwrap();
        let idx = items
            .iter()
            .position(|i| i.id == id)
            .ok_or_else(|| ToolError::InvalidArgs(format!("todo not found: {}", id)))?;
        items[idx].done = true;
        Ok(items[idx].clone())
    }

    /// List all todo items.
    pub fn list(&self) -> Vec<TodoItem> {
        self.items.lock().unwrap().clone()
    }

    /// Get a todo item by ID.
    pub fn get(&self, id: &str) -> Option<TodoItem> {
        self.items
            .lock()
            .unwrap()
            .iter()
            .find(|i| i.id == id)
            .cloned()
    }
}

/// Arguments for the todo tool.
#[derive(Debug, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum TodoArgs {
    Add {
        title: String,
    },
    Complete {
        id: String,
    },
    List,
    Delegate {
        /// ID of the todo item to delegate
        todo_id: String,
        /// Name of the agent profile to use
        agent: String,
        /// Task description/prompt for the sub-agent
        prompt: String,
    },
}

/// The todo tool implementation.
#[derive(Debug)]
pub struct TodoTool {
    pub list: TodoList,
}

impl TodoTool {
    /// Create a new todo tool with a shared list.
    pub fn new(list: TodoList) -> Self {
        Self { list }
    }

    /// Create a new todo tool with a new empty list.
    pub fn new_empty() -> Self {
        Self::new(TodoList::new())
    }
}

#[async_trait]
impl Tool for TodoTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "todo",
            "Manage a todo list: add, complete, list tasks, or delegate to sub-agent",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["add", "complete", "list", "delegate"]
                    },
                    "title": {"type": "string"},
                    "id": {"type": "string"},
                    "todo_id": {"type": "string"},
                    "agent": {"type": "string"},
                    "prompt": {"type": "string"}
                },
                "required": ["action"]
            }),
        )
    }

    async fn execute(&self, args: Value) -> Result<ToolExecutionResult, ToolError> {
        let parsed: TodoArgs = serde_json::from_value(args)
            .map_err(|e| ToolError::InvalidArgs(format!("invalid args: {}", e)))?;

        match parsed {
            TodoArgs::Add { title } => {
                let item = self.list.add(title);
                Ok(ToolExecutionResult::output(
                    serde_json::to_string(&item).unwrap(),
                ))
            }
            TodoArgs::Complete { id } => {
                let item = self.list.complete(&id)?;
                Ok(ToolExecutionResult::output(
                    serde_json::to_string(&item).unwrap(),
                ))
            }
            TodoArgs::List => {
                let items = self.list.list();
                Ok(ToolExecutionResult::output(
                    serde_json::to_string(&items).unwrap(),
                ))
            }
            TodoArgs::Delegate { .. } => {
                // This should be intercepted by ReactLoop::execute_tool before reaching here
                Err(ToolError::InvalidArgs(
                    "delegate action should be handled by ReactLoop, not TodoTool directly"
                        .to_string(),
                ))
            }
        }
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn test_todo_tool_add() {
        let tool = TodoTool::new_empty();
        let result = tool
            .execute(json!({"action": "add", "title": "Test task"}))
            .await;
        assert!(result.is_ok());
        let item: TodoItem = serde_json::from_str(&result.unwrap().output).unwrap();
        assert_eq!(item.title, "Test task");
        assert!(!item.done);
    }

    #[tokio::test]
    async fn test_todo_tool_complete() {
        let tool = TodoTool::new_empty();
        let add_result = tool
            .execute(json!({"action": "add", "title": "Test task"}))
            .await
            .unwrap();
        let item: TodoItem = serde_json::from_str(&add_result.output).unwrap();

        let complete_result = tool
            .execute(json!({"action": "complete", "id": item.id}))
            .await;
        assert!(complete_result.is_ok());
        let completed: TodoItem = serde_json::from_str(&complete_result.unwrap().output).unwrap();
        assert!(completed.done);
    }

    #[tokio::test]
    async fn test_todo_tool_list() {
        let tool = TodoTool::new_empty();
        tool.execute(json!({"action": "add", "title": "Task 1"}))
            .await
            .unwrap();
        tool.execute(json!({"action": "add", "title": "Task 2"}))
            .await
            .unwrap();

        let result = tool.execute(json!({"action": "list"})).await;
        assert!(result.is_ok());
        let items: Vec<TodoItem> = serde_json::from_str(&result.unwrap().output).unwrap();
        assert_eq!(items.len(), 2);
    }

    #[tokio::test]
    async fn test_todo_tool_complete_not_found() {
        let tool = TodoTool::new_empty();
        let result = tool
            .execute(json!({"action": "complete", "id": "nonexistent"}))
            .await;
        assert!(result.is_err());
    }

    #[test]
    fn test_todo_list_shared() {
        let list = TodoList::new();
        list.add("Task 1".into());
        list.add("Task 2".into());

        let tool1 = TodoTool::new(list.clone());
        let _tool2 = TodoTool::new(list);

        // They share the same list
        let items = tool1.list.list();
        assert_eq!(items.len(), 2);
    }
}
