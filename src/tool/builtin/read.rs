//! Built-in `read` tool — read file contents.
//!
//! Category: Read (no approval required).
//! Output is truncated to MAX_OUTPUT_BYTES with head/tail overflow indicator.

use crate::tool::builtin::{MAX_OUTPUT_BYTES, truncate_output};
use crate::tool::tool_trait::{Tool, ToolError, ToolExecutionResult, ToolSpec};
use async_trait::async_trait;
use serde_json::Value;

/// Built-in tool for reading file contents.
#[derive(Debug, Default)]
pub struct ReadTool;

impl ReadTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Tool for ReadTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "read",
            "Read the contents of a file.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path to the file to read."
                    }
                },
                "required": ["path"]
            }),
        )
    }

    async fn execute(&self, args: Value) -> Result<ToolExecutionResult, ToolError> {
        let path = args
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArgs("missing required argument 'path'".into()))?;

        let content = std::fs::read_to_string(path)
            .map_err(|e| ToolError::ExecutionFailed(format!("failed to read '{path}': {e}")))?;

        let (output, truncated) = truncate_output(&content, MAX_OUTPUT_BYTES);

        Ok(ToolExecutionResult {
            output,
            exit_code: None,
            timed_out: false,
            cancelled: false,
            truncated,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;
    use tempfile::tempdir;

    #[tokio::test]
    async fn test_read_tool_basic() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("test.txt");
        fs::write(&file_path, "Hello, World!").unwrap();

        let tool = ReadTool::new();
        let result = tool
            .execute(json!({"path": file_path.to_str().unwrap()}))
            .await
            .unwrap();

        assert!(!result.truncated);
        assert!(result.output.contains("Hello, World!"));
    }

    #[tokio::test]
    async fn test_read_tool_truncation() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("large.txt");
        let content = "x".repeat(20000);
        fs::write(&file_path, &content).unwrap();

        let tool = ReadTool::new();
        let result = tool
            .execute(json!({"path": file_path.to_str().unwrap()}))
            .await
            .unwrap();

        assert!(result.truncated);
        assert!(result.output.contains("[output truncated"));
        assert!(result.output.len() <= MAX_OUTPUT_BYTES + 100); // some overhead
    }

    #[tokio::test]
    async fn test_read_tool_missing_file() {
        let tool = ReadTool::new();
        let result = tool
            .execute(json!({"path": "/tmp/okeanos-nonexistent-file-12345"}))
            .await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_read_tool_missing_path_arg() {
        let tool = ReadTool::new();
        let result = tool.execute(json!({})).await;
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("missing required argument 'path'")
        );
    }
}
