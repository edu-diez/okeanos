//! Built-in `delete` tool — delete a file or directory.
//!
//! Category: Delete (requires approval).

use crate::tool::builtin::{MAX_OUTPUT_BYTES, truncate_output};
use crate::tool::tool_trait::{Tool, ToolError, ToolExecutionResult, ToolSpec};
use async_trait::async_trait;
use serde_json::Value;
use std::path::Path;

/// Built-in tool for deleting files or directories.
#[derive(Debug, Default)]
pub struct DeleteTool;

impl DeleteTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Tool for DeleteTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "delete",
            "Delete a file or directory.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path to the file or directory to delete."
                    }
                },
                "required": ["path"]
            }),
        )
        .with_category(crate::tool::tool_trait::ToolCategory::Delete)
    }

    async fn execute(&self, args: Value) -> Result<ToolExecutionResult, ToolError> {
        let path = args
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArgs("missing required argument 'path'".into()))?;

        let path = Path::new(path);

        if path.is_dir() {
            std::fs::remove_dir_all(path).map_err(|e| {
                ToolError::ExecutionFailed(format!(
                    "failed to delete directory '{}': {e}",
                    path.display()
                ))
            })?;
        } else {
            std::fs::remove_file(path).map_err(|e| {
                ToolError::ExecutionFailed(format!(
                    "failed to delete file '{}': {e}",
                    path.display()
                ))
            })?;
        }

        let output = format!("Deleted {}", path.display());
        let (output, truncated) = truncate_output(&output, MAX_OUTPUT_BYTES);

        Ok(ToolExecutionResult {
            output,
            exit_code: None,
            timed_out: false,
            cancelled: false,
            truncated,
        })
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;
    use tempfile::tempdir;

    #[tokio::test]
    async fn test_delete_tool_file() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("test.txt");
        fs::write(&file_path, "to be deleted").unwrap();

        let tool = DeleteTool::new();
        let result = tool
            .execute(json!({"path": file_path.to_str().unwrap()}))
            .await
            .unwrap();

        assert!(!result.truncated);
        assert!(!file_path.exists());
    }

    #[tokio::test]
    async fn test_delete_tool_directory() {
        let dir = tempdir().unwrap();
        let subdir = dir.path().join("subdir");
        fs::create_dir_all(&subdir).unwrap();
        fs::write(subdir.join("file.txt"), "content").unwrap();

        let tool = DeleteTool::new();
        let result = tool
            .execute(json!({"path": subdir.to_str().unwrap()}))
            .await
            .unwrap();

        assert!(!result.truncated);
        assert!(!subdir.exists());
    }

    #[tokio::test]
    async fn test_delete_tool_nonexistent() {
        let tool = DeleteTool::new();
        let result = tool
            .execute(json!({"path": "/tmp/okeanos-nonexistent-12345"}))
            .await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_delete_tool_missing_path_arg() {
        let tool = DeleteTool::new();
        let result = tool.execute(json!({})).await;
        assert!(result.is_err());
    }
}
