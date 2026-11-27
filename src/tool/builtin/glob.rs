//! Built-in `glob` tool — find files by name pattern.
//!
//! Category: Read (no approval required). Honors `.gitignore`.

use crate::tool::tool_trait::{Tool, ToolError, ToolExecutionResult, ToolSpec};
use async_trait::async_trait;
use ignore::WalkBuilder;
use ignore::overrides::OverrideBuilder;
use serde_json::Value;
use std::path::PathBuf;

const MAX_RESULTS: usize = 500;

/// Built-in tool for listing files that match a glob pattern.
#[derive(Debug, Default)]
pub struct GlobTool;

impl GlobTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Tool for GlobTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "glob",
            "Find files by glob pattern (e.g. \"**/*.rs\", \"src/**/mod.rs\"). \
             Respects .gitignore. Returns paths sorted alphabetically.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "pattern": {
                        "type": "string",
                        "description": "Glob pattern to match file paths against."
                    },
                    "path": {
                        "type": "string",
                        "description": "Directory to search (defaults to current directory)."
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
            .ok_or_else(|| ToolError::InvalidArgs("missing required argument 'pattern'".into()))?
            .to_string();
        let root = PathBuf::from(args.get("path").and_then(|v| v.as_str()).unwrap_or("."));
        if !root.is_dir() {
            return Err(ToolError::ExecutionFailed(format!(
                "'{}' is not a directory",
                root.display()
            )));
        }

        let found = {
            let pattern = pattern.clone();
            tokio::task::spawn_blocking(move || find(&root, &pattern))
                .await
                .map_err(|e| ToolError::ExecutionFailed(format!("glob task failed: {e}")))??
        };

        let truncated = found.len() > MAX_RESULTS;
        let mut output = if found.is_empty() {
            format!("No files match '{pattern}'")
        } else {
            found[..found.len().min(MAX_RESULTS)].join("\n")
        };
        if truncated {
            output.push_str(&format!(
                "\n[{} more files not shown; narrow the pattern]",
                found.len() - MAX_RESULTS
            ));
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

fn find(root: &PathBuf, pattern: &str) -> Result<Vec<String>, ToolError> {
    let mut ob = OverrideBuilder::new(root);
    ob.add(pattern)
        .map_err(|e| ToolError::InvalidArgs(format!("invalid glob '{pattern}': {e}")))?;
    let overrides = ob
        .build()
        .map_err(|e| ToolError::InvalidArgs(format!("invalid glob '{pattern}': {e}")))?;

    let mut walker = WalkBuilder::new(root);
    walker
        .hidden(false)
        .require_git(false)
        .overrides(overrides)
        .filter_entry(|e| e.file_name() != ".git");

    let mut out: Vec<String> = walker
        .build()
        .flatten()
        .filter(|e| e.file_type().is_some_and(|t| t.is_file()))
        .map(|e| e.path().display().to_string().replace('\\', "/"))
        .collect();
    out.sort();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;
    use tempfile::tempdir;

    #[tokio::test]
    async fn test_glob_matches_recursively() {
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src/sub")).unwrap();
        fs::write(dir.path().join("src/a.rs"), "").unwrap();
        fs::write(dir.path().join("src/sub/b.rs"), "").unwrap();
        fs::write(dir.path().join("src/c.txt"), "").unwrap();

        let result = GlobTool::new()
            .execute(json!({"pattern": "**/*.rs", "path": dir.path().to_str().unwrap()}))
            .await
            .unwrap();
        assert!(result.output.contains("a.rs"));
        assert!(result.output.contains("b.rs"));
        assert!(!result.output.contains("c.txt"));
    }

    #[tokio::test]
    async fn test_glob_no_match() {
        let dir = tempdir().unwrap();
        let result = GlobTool::new()
            .execute(json!({"pattern": "*.zzz", "path": dir.path().to_str().unwrap()}))
            .await
            .unwrap();
        assert!(result.output.contains("No files match"));
    }
}
