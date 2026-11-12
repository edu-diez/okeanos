//! Tool trait and registry.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::fmt;
use thiserror::Error;

/// Category of a tool operation, used by the permission layer to decide
/// whether an operation needs approval before execution.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum ToolCategory {
    /// Read-only operation (read, grep, todo list, etc.) — safe by default.
    #[default]
    Read,
    /// Write/modify operation (write, edit, etc.) — requires confirmation.
    Write,
    /// Delete operation — requires confirmation.
    Delete,
    /// Network operation — requires confirmation.
    Network,
}

/// JSON Schema for tool input.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    pub output_schema: Option<Value>,
    /// Operation category for permission gating.
    pub category: ToolCategory,
}

impl ToolSpec {
    /// Create a new tool spec (defaults to `Read` category).
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        input_schema: Value,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            input_schema,
            output_schema: None,
            category: ToolCategory::Read,
        }
    }

    /// Create a tool spec with output schema.
    pub fn with_output_schema(mut self, output_schema: Value) -> Self {
        self.output_schema = Some(output_schema);
        self
    }

    /// Set the tool's operation category.
    pub fn with_category(mut self, category: ToolCategory) -> Self {
        self.category = category;
        self
    }
}

/// Errors from tool execution.
#[derive(Debug, Error)]
pub enum ToolError {
    #[error("invalid arguments: {0}")]
    InvalidArgs(String),
    #[error("execution failed: {0}")]
    ExecutionFailed(String),
    #[error("permission denied: {0}")]
    PermissionDenied(String),
    #[error("tool not found: {0}")]
    NotFound(String),
}

/// Result of a tool execution.
///
/// Contains the tool's output plus metadata about how the execution
/// concluded (exit code, timeout, cancellation, truncation).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolExecutionResult {
    /// The tool's output text.
    pub output: String,
    /// Process exit code (only set for `run` tool; `None` for other tools or if killed by signal).
    pub exit_code: Option<i32>,
    /// Whether execution was terminated due to a timeout.
    pub timed_out: bool,
    /// Whether execution was cancelled (e.g., by user interrupt).
    pub cancelled: bool,
    /// Whether the output was truncated due to size limits.
    pub truncated: bool,
}

impl ToolExecutionResult {
    /// Create a successful result with just output.
    pub fn output(output: impl Into<String>) -> Self {
        Self {
            output: output.into(),
            exit_code: None,
            timed_out: false,
            cancelled: false,
            truncated: false,
        }
    }
}

/// The Tool trait — all tools implement this.
#[async_trait]
pub trait Tool: Send + Sync {
    /// Get the tool specification (JSON schema).
    fn spec(&self) -> ToolSpec;

    /// Execute the tool with the given arguments.
    ///
    /// Returns a [`ToolExecutionResult`] containing the output plus
    /// metadata (exit code, timeout, cancellation, truncation).
    async fn execute(&self, args: Value) -> Result<ToolExecutionResult, ToolError>;
}

/// Registry of available tools.
pub struct ToolRegistry {
    tools: HashMap<String, Box<dyn Tool>>,
}

impl fmt::Debug for ToolRegistry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ToolRegistry")
            .field("tools", &self.tools.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolRegistry {
    /// Create a new empty registry.
    pub fn new() -> Self {
        Self {
            tools: HashMap::new(),
        }
    }

    /// Register a tool.
    pub fn register<T: Tool + 'static>(&mut self, tool: T) {
        let spec = tool.spec();
        self.tools.insert(spec.name.clone(), Box::new(tool));
    }

    /// Get a tool by name.
    pub fn get(&self, name: &str) -> Option<&dyn Tool> {
        self.tools.get(name).map(|b| b.as_ref())
    }

    /// Get all tool specs.
    pub fn specs(&self) -> Vec<ToolSpec> {
        self.tools.values().map(|t| t.spec()).collect()
    }

    /// Get tool names.
    pub fn names(&self) -> Vec<String> {
        self.tools.keys().cloned().collect()
    }

    /// Get specs for only the named tools, in the order given.
    /// Tools not found in the registry are skipped.
    pub fn filtered_specs(&self, names: &[String]) -> Vec<ToolSpec> {
        names
            .iter()
            .filter_map(|name| self.get(name).map(|tool| tool.spec()))
            .collect()
    }

    /// Check if a tool exists.
    pub fn has(&self, name: &str) -> bool {
        self.tools.contains_key(name)
    }

    /// Remove a tool.
    pub fn remove(&mut self, name: &str) -> Option<Box<dyn Tool>> {
        self.tools.remove(name)
    }

    /// Get the number of registered tools.
    pub fn len(&self) -> usize {
        self.tools.len()
    }

    /// Check if registry is empty.
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use serde_json::json;

    struct EchoTool;

    #[async_trait]
    impl Tool for EchoTool {
        fn spec(&self) -> ToolSpec {
            ToolSpec::new(
                "echo",
                "Echo the input",
                json!({
                    "type": "object",
                    "properties": {
                        "message": {"type": "string"}
                    },
                    "required": ["message"]
                }),
            )
        }

        async fn execute(&self, args: Value) -> Result<ToolExecutionResult, ToolError> {
            let message = args
                .get("message")
                .and_then(|v| v.as_str())
                .ok_or_else(|| ToolError::InvalidArgs("missing message".into()))?;
            Ok(ToolExecutionResult::output(message.to_string()))
        }
    }

    #[test]
    fn test_tool_registry() {
        let mut registry = ToolRegistry::new();
        assert!(registry.is_empty());

        registry.register(EchoTool);
        assert_eq!(registry.len(), 1);
        assert!(registry.has("echo"));

        let spec = registry.get("echo").unwrap().spec();
        assert_eq!(spec.name, "echo");

        let specs = registry.specs();
        assert_eq!(specs.len(), 1);
    }

    #[tokio::test]
    async fn test_tool_execution() {
        let tool = EchoTool;
        let result = tool.execute(json!({"message": "hello"})).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap().output, "hello");
    }

    #[tokio::test]
    async fn test_tool_execution_invalid_args() {
        let tool = EchoTool;
        let result = tool.execute(json!({})).await;
        assert!(result.is_err());
    }
}
