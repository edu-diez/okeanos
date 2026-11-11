//! Built-in `grep` tool — search for patterns in files.
//!
//! Category: Read (no approval required).
//! Output is truncated to MAX_OUTPUT_BYTES with head/tail overflow indicator.

use crate::tool::builtin::{MAX_OUTPUT_BYTES, truncate_output};
use crate::tool::tool_trait::{Tool, ToolError, ToolExecutionResult, ToolSpec};
use async_trait::async_trait;
use regex::Regex;
use serde_json::Value;
use std::path::Path;

/// Built-in tool for searching file contents with a regex pattern.
#[derive(Debug, Default)]
pub struct GrepTool;

impl GrepTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Tool for GrepTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "grep",
            "Search for a regex pattern in files under a directory.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "pattern": {
                        "type": "string",
                        "description": "Regular expression pattern to search for."
                    },
                    "path": {
                        "type": "string",
                        "description": "Directory or file to search (defaults to current directory)."
                    }
                },
                "required": ["pattern"]
            }),
        )
    }

    async fn execute(&self, args: Value) -> Result<ToolExecutionResult, ToolError> {
        let pattern = args
            .get("pattern")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArgs("missing required argument 'pattern'".into()))?;

        let path = args.get("path").and_then(|v| v.as_str()).unwrap_or(".");

        let regex = Regex::new(pattern).map_err(|e| {
            ToolError::InvalidArgs(format!("invalid regex pattern '{pattern}': {e}"))
        })?;

        let path = Path::new(path);
        let mut results = Vec::new();

        if path.is_file() {
            search_file(path, &regex, &mut results)?;
        } else if path.is_dir() {
            search_dir(path, &regex, &mut results)?;
        } else {
            return Err(ToolError::ExecutionFailed(format!(
                "path '{}' is not a file or directory",
                path.display()
            )));
        }

        let output = if results.is_empty() {
            format!(
                "No matches found for pattern '{pattern}' in '{}'",
                path.display()
            )
        } else {
            results.join("\n")
        };

        let (output, truncated) = truncate_output(&output, MAX_OUTPUT_BYTES);

        Ok(ToolExecutionResult {
            output,
            exit_code: None,
            timed_out: false,
            cancelled: false,
            truncated,
        })
    }
}

/// Search a single file for regex matches.
fn search_file(path: &Path, regex: &Regex, results: &mut Vec<String>) -> Result<(), ToolError> {
    let content = std::fs::read_to_string(path).map_err(|e| {
        ToolError::ExecutionFailed(format!("failed to read '{}': {e}", path.display()))
    })?;

    for (line_num, line) in content.lines().enumerate() {
        if regex.is_match(line) {
            results.push(format!("{}:{}: {}", path.display(), line_num + 1, line));
        }
    }
    Ok(())
}

/// Recursively search a directory for regex matches.
fn search_dir(dir: &Path, regex: &Regex, results: &mut Vec<String>) -> Result<(), ToolError> {
    let entries = std::fs::read_dir(dir).map_err(|e| {
        ToolError::ExecutionFailed(format!("failed to read directory '{}': {e}", dir.display()))
    })?;

    for entry in entries {
        let entry = entry.map_err(|e| {
            ToolError::ExecutionFailed(format!("failed to read directory entry: {e}"))
        })?;
        let path = entry.path();

        if path.is_dir() {
            search_dir(&path, regex, results)?;
        } else if path.is_file() {
            // Skip binary-ish files and common non-text directories
            let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if file_name.starts_with('.') || file_name == "target" || file_name == "node_modules" {
                continue;
            }
            search_file(&path, regex, results)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;
    use tempfile::tempdir;

    #[tokio::test]
    async fn test_grep_tool_basic() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("foo.rs"), "fn login() {}\nfn logout() {}\n").unwrap();
        fs::write(dir.path().join("bar.rs"), "fn process() {}\n").unwrap();

        let tool = GrepTool::new();
        let result = tool
            .execute(json!({
                "pattern": "login",
                "path": dir.path().to_str().unwrap()
            }))
            .await
            .unwrap();

        assert!(!result.truncated);
        assert!(result.output.contains("login"));
        assert!(result.output.contains("foo.rs"));
    }

    #[tokio::test]
    async fn test_grep_tool_no_matches() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("foo.rs"), "fn foo() {}\n").unwrap();

        let tool = GrepTool::new();
        let result = tool
            .execute(json!({
                "pattern": "nonexistent_pattern_xyz",
                "path": dir.path().to_str().unwrap()
            }))
            .await
            .unwrap();

        assert!(!result.truncated);
        assert!(result.output.contains("No matches found"));
    }

    #[tokio::test]
    async fn test_grep_tool_single_file() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("test.txt");
        fs::write(&file_path, "alpha\nbeta\ngamma\n").unwrap();

        let tool = GrepTool::new();
        let result = tool
            .execute(json!({
                "pattern": "beta",
                "path": file_path.to_str().unwrap()
            }))
            .await
            .unwrap();

        assert!(!result.truncated);
        assert!(result.output.contains("beta"));
    }

    #[tokio::test]
    async fn test_grep_tool_invalid_regex() {
        let tool = GrepTool::new();
        let result = tool.execute(json!({"pattern": "[invalid"})).await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_grep_tool_missing_pattern() {
        let tool = GrepTool::new();
        let result = tool.execute(json!({})).await;
        assert!(result.is_err());
    }
}
