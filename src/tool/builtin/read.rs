//! Built-in `read` tool — read file contents with line numbers.
//!
//! Category: Read (no approval required).
//! Output is line-numbered (`cat -n` style) and windowed with `offset`/`limit`
//! so large files are paged instead of head/tail-truncated.

use crate::tool::builtin::fsio;
use crate::tool::tool_trait::{Tool, ToolError, ToolExecutionResult, ToolSpec};
use async_trait::async_trait;
use serde_json::Value;

/// Default number of lines returned when `limit` is not given.
const DEFAULT_LIMIT: usize = 2000;
/// Lines longer than this are cut (minified files, lockfiles).
const MAX_LINE_CHARS: usize = 2000;
/// Hard byte cap on a single read result.
const MAX_READ_BYTES: usize = 50_000;

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
            "Read a file. Output is prefixed with line numbers. Reads up to 2000 lines \
             by default; use `offset` and `limit` to page through larger files.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path to the file to read."
                    },
                    "offset": {
                        "type": "integer",
                        "description": "1-based line number to start reading from (default 1)."
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Maximum number of lines to read (default 2000)."
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
        let offset = args
            .get("offset")
            .and_then(|v| v.as_u64())
            .map_or(1, |v| v.max(1) as usize);
        let limit = args
            .get("limit")
            .and_then(|v| v.as_u64())
            .map_or(DEFAULT_LIMIT, |v| {
                (v.max(1) as usize).min(DEFAULT_LIMIT * 5)
            });

        crate::tool::workspace::guard_read(std::path::Path::new(path))?;
        let path_buf = std::path::PathBuf::from(path);
        let bytes = tokio::task::spawn_blocking(move || fsio::read_existing(&path_buf))
            .await
            .map_err(|e| ToolError::ExecutionFailed(format!("read task failed: {e}")))??
            .ok_or_else(|| {
                ToolError::ExecutionFailed(format!("failed to read '{path}': file not found"))
            })?;
        if bytes[..bytes.len().min(8192)].contains(&0) {
            return Err(ToolError::ExecutionFailed(format!(
                "'{path}' looks like a binary file ({} bytes)",
                bytes.len()
            )));
        }
        fsio::record(std::path::Path::new(path), &bytes);
        let content = String::from_utf8_lossy(&bytes);

        let (mut output, truncated) = format_window(&content, offset, limit);
        if matches!(content, std::borrow::Cow::Owned(_)) {
            output.push_str("\n[note: file is not valid UTF-8; invalid bytes shown as U+FFFD and edit will refuse to modify it]");
        }
        Ok(ToolExecutionResult {
            output,
            exit_code: None,
            timed_out: false,
            cancelled: false,
            truncated,
            images: Vec::new(),
        })
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// Render `limit` lines starting at 1-based `offset` with line numbers.
/// Returns the text and whether more content remains beyond the window.
fn format_window(content: &str, offset: usize, limit: usize) -> (String, bool) {
    let total = content.lines().count();
    if total == 0 {
        return ("(empty file)".to_string(), false);
    }
    if offset > total {
        return (
            format!("(offset {offset} is past the end of the file; it has {total} lines)"),
            false,
        );
    }

    let mut out = String::new();
    let mut next = offset;
    for (i, line) in content.lines().enumerate().skip(offset - 1).take(limit) {
        let line = if line.chars().count() > MAX_LINE_CHARS {
            let cut: String = line.chars().take(MAX_LINE_CHARS).collect();
            format!("{cut}… [line truncated]")
        } else {
            line.to_string()
        };
        let row = format!("{:>6}\t{line}\n", i + 1);
        if out.len() + row.len() > MAX_READ_BYTES && !out.is_empty() {
            break;
        }
        out.push_str(&row);
        next = i + 2;
    }

    let truncated = next <= total;
    if truncated {
        out.push_str(&format!(
            "[showing lines {offset}-{} of {total}; call read again with offset={next} to continue]",
            next - 1
        ));
    }
    (out, truncated)
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

        let result = ReadTool::new()
            .execute(json!({"path": file_path.to_str().unwrap()}))
            .await
            .unwrap();

        assert!(!result.truncated);
        assert!(result.output.contains("     1\tHello, World!"));
    }

    #[tokio::test]
    async fn test_read_tool_offset_limit() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("many.txt");
        let content: String = (1..=100).map(|i| format!("line{i}\n")).collect();
        fs::write(&file_path, content).unwrap();

        let result = ReadTool::new()
            .execute(json!({"path": file_path.to_str().unwrap(), "offset": 10, "limit": 5}))
            .await
            .unwrap();

        assert!(result.truncated);
        assert!(result.output.contains("    10\tline10"));
        assert!(result.output.contains("    14\tline14"));
        assert!(!result.output.contains("line15\n"));
        assert!(result.output.contains("offset=15"));
    }

    #[tokio::test]
    async fn test_read_tool_long_line_and_byte_cap() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("large.txt");
        fs::write(&file_path, "x".repeat(20000)).unwrap();

        let result = ReadTool::new()
            .execute(json!({"path": file_path.to_str().unwrap()}))
            .await
            .unwrap();

        assert!(result.output.contains("[line truncated]"));
        assert!(result.output.len() < 3000);
    }

    #[tokio::test]
    async fn test_read_tool_binary_rejected() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("bin.dat");
        fs::write(&file_path, [0u8, 1, 2, 3]).unwrap();

        let result = ReadTool::new()
            .execute(json!({"path": file_path.to_str().unwrap()}))
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_read_tool_missing_file() {
        let result = ReadTool::new()
            .execute(json!({"path": "/tmp/okeanos-nonexistent-file-12345"}))
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_read_tool_missing_path_arg() {
        let result = ReadTool::new().execute(json!({})).await;
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("missing required argument 'path'")
        );
    }
}
