//! Built-in `grep` tool — search file contents with a regex.
//!
//! Category: Read (no approval required).
//! Honors `.gitignore`, skips binary files and `.git`, supports a glob filter,
//! context lines, case-insensitivity and a match cap.

use crate::tool::builtin::{MAX_OUTPUT_BYTES, truncate_output_with_spill};
use crate::tool::tool_trait::{Tool, ToolError, ToolExecutionResult, ToolSpec};
use async_trait::async_trait;
use ignore::WalkBuilder;
use ignore::overrides::OverrideBuilder;
use regex::RegexBuilder;
use serde_json::Value;
use std::path::{Path, PathBuf};

const DEFAULT_HEAD_LIMIT: usize = 200;

/// Built-in tool for searching file contents with a regex pattern.
#[derive(Debug, Default)]
pub struct GrepTool;

impl GrepTool {
    pub fn new() -> Self {
        Self
    }
}

struct Params {
    pattern: String,
    path: PathBuf,
    glob: Option<String>,
    context: usize,
    ignore_case: bool,
    head_limit: usize,
}

#[async_trait]
impl Tool for GrepTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "grep",
            "Search file contents for a regex. Respects .gitignore and skips binary files. \
             Output lines are `path:line: text` (context lines use `-` separators).",
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
                    },
                    "glob": {
                        "type": "string",
                        "description": "Only search files matching this glob, e.g. \"*.rs\" or \"src/**/*.ts\"."
                    },
                    "context": {
                        "type": "integer",
                        "description": "Lines of context to show around each match (default 0)."
                    },
                    "ignore_case": {
                        "type": "boolean",
                        "description": "Case-insensitive search (default false)."
                    },
                    "head_limit": {
                        "type": "integer",
                        "description": "Maximum number of matches to return (default 200)."
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
        let params = Params {
            path: PathBuf::from(args.get("path").and_then(|v| v.as_str()).unwrap_or(".")),
            glob: args.get("glob").and_then(|v| v.as_str()).map(String::from),
            context: args.get("context").and_then(|v| v.as_u64()).unwrap_or(0) as usize,
            ignore_case: args
                .get("ignore_case")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            head_limit: args
                .get("head_limit")
                .and_then(|v| v.as_u64())
                .map_or(DEFAULT_HEAD_LIMIT, |v| v.max(1) as usize),
            pattern,
        };

        // Validate cheaply up front so bad input is an InvalidArgs error.
        RegexBuilder::new(&params.pattern)
            .case_insensitive(params.ignore_case)
            .build()
            .map_err(|e| {
                ToolError::InvalidArgs(format!("invalid regex pattern '{}': {e}", params.pattern))
            })?;
        if !params.path.exists() {
            return Err(ToolError::ExecutionFailed(format!(
                "path '{}' is not a file or directory",
                params.path.display()
            )));
        }

        let desc = format!(
            "pattern '{}' in '{}'",
            params.pattern,
            params.path.display()
        );
        let (lines, capped) = tokio::task::spawn_blocking(move || search(&params))
            .await
            .map_err(|e| ToolError::ExecutionFailed(format!("search task failed: {e}")))??;

        let mut output = if lines.is_empty() {
            format!("No matches found for {desc}")
        } else {
            lines.join("\n")
        };
        if capped {
            output.push_str("\n[match limit reached; narrow the pattern/path or raise head_limit]");
        }

        let (output, truncated) = truncate_output_with_spill(&output, MAX_OUTPUT_BYTES);
        Ok(ToolExecutionResult {
            output,
            exit_code: None,
            timed_out: false,
            cancelled: false,
            truncated: truncated || capped,
        })
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

fn search(p: &Params) -> Result<(Vec<String>, bool), ToolError> {
    let regex = RegexBuilder::new(&p.pattern)
        .case_insensitive(p.ignore_case)
        .build()
        .map_err(|e| ToolError::InvalidArgs(e.to_string()))?;

    let mut walker = WalkBuilder::new(&p.path);
    // Dotfiles like `.github/` are worth searching; `.git` is excluded below.
    walker.hidden(false).require_git(false);
    walker.filter_entry(|e| e.file_name() != ".git");
    if let Some(glob) = &p.glob {
        let mut ob = OverrideBuilder::new(&p.path);
        ob.add(glob)
            .map_err(|e| ToolError::InvalidArgs(format!("invalid glob '{glob}': {e}")))?;
        walker.overrides(
            ob.build()
                .map_err(|e| ToolError::InvalidArgs(format!("invalid glob '{glob}': {e}")))?,
        );
    }

    let mut results = Vec::new();
    let mut matches = 0usize;
    let mut capped = false;

    'files: for entry in walker.build().flatten() {
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let Ok(bytes) = std::fs::read(entry.path()) else {
            continue;
        };
        if bytes[..bytes.len().min(8192)].contains(&0) {
            continue;
        }
        let content = String::from_utf8_lossy(&bytes);
        let lines: Vec<&str> = content.lines().collect();
        let mut last_printed: Option<usize> = None;

        for (i, line) in lines.iter().enumerate() {
            if !regex.is_match(line) {
                continue;
            }
            if matches >= p.head_limit {
                capped = true;
                break 'files;
            }
            matches += 1;

            let start = i.saturating_sub(p.context);
            let end = (i + p.context).min(lines.len() - 1);
            let from = last_printed.map_or(start, |l| start.max(l + 1));
            if p.context > 0 && last_printed.is_some_and(|l| from > l + 1) {
                results.push("--".to_string());
            }
            for (j, text) in lines.iter().enumerate().take(end + 1).skip(from) {
                let sep = if j == i { ':' } else { '-' };
                results.push(format!("{}:{}{sep} {}", display(entry.path()), j + 1, text));
            }
            last_printed = Some(end);
        }
    }
    Ok((results, capped))
}

fn display(path: &Path) -> String {
    path.display().to_string().replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;
    use tempfile::tempdir;

    async fn run(args: Value) -> Result<ToolExecutionResult, ToolError> {
        GrepTool::new().execute(args).await
    }

    #[tokio::test]
    async fn test_grep_tool_basic() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("foo.rs"), "fn login() {}\nfn logout() {}\n").unwrap();
        fs::write(dir.path().join("bar.rs"), "fn process() {}\n").unwrap();

        let result = run(json!({"pattern": "login", "path": dir.path().to_str().unwrap()}))
            .await
            .unwrap();
        assert!(!result.truncated);
        assert!(result.output.contains("foo.rs:1: fn login"));
        assert!(!result.output.contains("bar.rs"));
    }

    #[tokio::test]
    async fn test_grep_tool_no_matches() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("foo.rs"), "fn foo() {}\n").unwrap();
        let result =
            run(json!({"pattern": "nonexistent_xyz", "path": dir.path().to_str().unwrap()}))
                .await
                .unwrap();
        assert!(result.output.contains("No matches found"));
    }

    #[tokio::test]
    async fn test_grep_tool_single_file() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("test.txt");
        fs::write(&file, "alpha\nbeta\ngamma\n").unwrap();
        let result = run(json!({"pattern": "beta", "path": file.to_str().unwrap()}))
            .await
            .unwrap();
        assert!(result.output.contains("beta"));
    }

    #[tokio::test]
    async fn test_grep_glob_context_and_case() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("a.rs"), "one\nTWO\nthree\nfour\n").unwrap();
        fs::write(dir.path().join("b.txt"), "two\n").unwrap();
        let result = run(json!({
            "pattern": "two", "path": dir.path().to_str().unwrap(),
            "glob": "*.rs", "context": 1, "ignore_case": true
        }))
        .await
        .unwrap();
        assert!(result.output.contains("a.rs:2: TWO"));
        assert!(result.output.contains("a.rs:1- one"));
        assert!(result.output.contains("a.rs:3- three"));
        assert!(!result.output.contains("b.txt"));
    }

    #[tokio::test]
    async fn test_grep_respects_gitignore_and_skips_binary() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join(".ignore"), "skipped.txt\n").unwrap();
        fs::write(dir.path().join("skipped.txt"), "needle\n").unwrap();
        fs::write(dir.path().join("bin.dat"), b"needle\0\0").unwrap();
        fs::create_dir(dir.path().join(".github")).unwrap();
        fs::write(dir.path().join(".github").join("ci.yml"), "needle\n").unwrap();
        let result = run(json!({"pattern": "needle", "path": dir.path().to_str().unwrap()}))
            .await
            .unwrap();
        assert!(result.output.contains("ci.yml"));
        assert!(!result.output.contains("skipped.txt"));
        assert!(!result.output.contains("bin.dat"));
    }

    #[tokio::test]
    async fn test_grep_head_limit() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), "x\nx\nx\nx\n").unwrap();
        let result =
            run(json!({"pattern": "x", "path": dir.path().to_str().unwrap(), "head_limit": 2}))
                .await
                .unwrap();
        assert!(result.truncated);
        assert!(result.output.contains("match limit reached"));
    }

    #[tokio::test]
    async fn test_grep_tool_invalid_regex() {
        assert!(run(json!({"pattern": "[invalid"})).await.is_err());
    }

    #[tokio::test]
    async fn test_grep_tool_missing_pattern() {
        assert!(run(json!({})).await.is_err());
    }
}
