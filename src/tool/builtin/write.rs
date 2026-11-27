//! Built-in `write` tool — create or overwrite a file.
//!
//! Category: Write (requires approval).
//! Output is truncated to MAX_OUTPUT_BYTES.

use crate::tool::builtin::{MAX_OUTPUT_BYTES, fsio, truncate_output};
use crate::tool::tool_trait::{Tool, ToolError, ToolExecutionResult, ToolSpec};
use async_trait::async_trait;
use serde_json::Value;
use std::path::Path;

/// Built-in tool for writing file contents.
#[derive(Debug, Default)]
pub struct WriteTool;

impl WriteTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Tool for WriteTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "write",
            "Create or overwrite a file at the given path.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path to the file to write."
                    },
                    "content": {
                        "type": "string",
                        "description": "Content to write to the file."
                    }
                },
                "required": ["path", "content"]
            }),
        )
        .with_category(crate::tool::tool_trait::ToolCategory::Write)
    }

    async fn execute(&self, args: Value) -> Result<ToolExecutionResult, ToolError> {
        let path = args
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArgs("missing required argument 'path'".into()))?;
        let content = args
            .get("content")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArgs("missing required argument 'content'".into()))?;

        let path = Path::new(path);
        crate::tool::workspace::guard_write(path)?;
        let write_path = path.to_path_buf();
        let bytes = content.as_bytes().to_vec();
        tokio::task::spawn_blocking(move || {
            if let Some(existing) = fsio::read_existing(&write_path)? {
                fsio::check_unchanged(&write_path, &existing)?;
            }
            fsio::atomic_write(&write_path, &bytes).map_err(|e| {
                ToolError::ExecutionFailed(format!(
                    "failed to write '{}': {e}",
                    write_path.display()
                ))
            })?;
            fsio::record(&write_path, &bytes);
            Ok::<_, ToolError>(())
        })
        .await
        .map_err(|e| ToolError::ExecutionFailed(format!("write task failed: {e}")))??;

        let output = format!("Wrote {} bytes to {}", content.len(), path.display());
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
    async fn test_write_tool_basic() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("test.txt");

        let tool = WriteTool::new();
        let result = tool
            .execute(json!({
                "path": file_path.to_str().unwrap(),
                "content": "Hello, World!"
            }))
            .await
            .unwrap();

        assert!(!result.truncated);
        assert!(result.output.contains("Wrote"));

        let content = fs::read_to_string(&file_path).unwrap();
        assert_eq!(content, "Hello, World!");
    }

    #[tokio::test]
    async fn test_write_tool_creates_parent_dirs() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("subdir").join("nested").join("test.txt");

        let tool = WriteTool::new();
        let result = tool
            .execute(json!({
                "path": file_path.to_str().unwrap(),
                "content": "nested content"
            }))
            .await
            .unwrap();

        assert!(!result.truncated);
        let content = fs::read_to_string(&file_path).unwrap();
        assert_eq!(content, "nested content");
    }

    #[tokio::test]
    async fn test_write_tool_overwrites() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("test.txt");
        fs::write(&file_path, "old content").unwrap();

        let tool = WriteTool::new();
        let result = tool
            .execute(json!({
                "path": file_path.to_str().unwrap(),
                "content": "new content"
            }))
            .await
            .unwrap();

        assert!(!result.truncated);
        let content = fs::read_to_string(&file_path).unwrap();
        assert_eq!(content, "new content");
    }

    #[tokio::test]
    async fn test_write_tool_missing_args() {
        let tool = WriteTool::new();
        let result = tool.execute(json!({"path": "/tmp/x"})).await;
        assert!(result.is_err());

        let result = tool.execute(json!({"content": "hello"})).await;
        assert!(result.is_err());
    }
}
