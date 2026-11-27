//! Built-in `edit` tool — modify files using diff/patch, whole-file, or str_replace format.
//!
//! Category: Write (requires approval).
//! Output is truncated to MAX_OUTPUT_BYTES.

use crate::tool::builtin::{MAX_OUTPUT_BYTES, fsio, truncate_output};
use crate::tool::tool_trait::{Tool, ToolError, ToolExecutionResult, ToolSpec};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::Path;

/// Edit format for the edit tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EditFormat {
    /// Unified diff/patch format (default for frontier models)
    #[default]
    Diff,
    /// Whole-file replacement format
    WholeFile,
    /// String replace format (find/replace)
    StrReplace,
}

impl EditFormat {
    /// Get the default edit format for a model tier.
    pub fn default_for_tier(tier: crate::model::ModelTier) -> Self {
        match tier {
            crate::model::ModelTier::Frontier => EditFormat::Diff,
            crate::model::ModelTier::Mid => EditFormat::Diff,
            crate::model::ModelTier::Small => EditFormat::WholeFile,
        }
    }
}

impl std::str::FromStr for EditFormat {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "diff" => Ok(EditFormat::Diff),
            "whole_file" => Ok(EditFormat::WholeFile),
            "str_replace" => Ok(EditFormat::StrReplace),
            _ => Err(format!("invalid edit format: {}", s)),
        }
    }
}

/// A single edit operation.
#[derive(Debug, Clone)]
enum EditOp {
    /// Unified diff/patch
    Diff(String),
    /// Whole file replacement
    WholeFile(String),
    /// String replace (old_str -> new_str)
    StrReplace { old_str: String, new_str: String },
}

/// Built-in tool for editing files.
#[derive(Debug, Default)]
pub struct EditTool {
    /// The edit format to use (can be overridden per invocation via args)
    default_format: EditFormat,
}

impl EditTool {
    pub fn new() -> Self {
        Self {
            default_format: EditFormat::Diff,
        }
    }

    /// Create an edit tool with a specific default format.
    pub fn with_format(format: EditFormat) -> Self {
        Self {
            default_format: format,
        }
    }
}

#[async_trait]
impl Tool for EditTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "edit",
            "Edit a file using diff/patch, whole-file replacement, or string replace.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path to the file to edit."
                    },
                    "format": {
                        "type": "string",
                        "description": "Edit format: 'diff', 'whole_file', or 'str_replace'. Defaults to model's preferred format.",
                        "enum": ["diff", "whole_file", "str_replace"]
                    },
                    "content": {
                        "type": "string",
                        "description": "For 'diff': unified diff/patch content. For 'whole_file': new file content. For 'str_replace': not used directly."
                    },
                    "old_str": {
                        "type": "string",
                        "description": "For 'str_replace': the text to find and replace."
                    },
                    "new_str": {
                        "type": "string",
                        "description": "For 'str_replace': the replacement text."
                    }
                },
                "required": ["path"],
                "oneOf": [
                    { "required": ["content"] },  // diff or whole_file
                    { "required": ["old_str", "new_str"] }  // str_replace
                ]
            }),
        )
        .with_category(crate::tool::tool_trait::ToolCategory::Write)
    }

    async fn execute(&self, args: Value) -> Result<ToolExecutionResult, ToolError> {
        let path = args
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArgs("missing required argument 'path'".into()))?;

        let path = Path::new(path);
        crate::tool::workspace::guard_write(path)?;

        // Determine the edit format
        let format = args
            .get("format")
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse::<EditFormat>().ok())
            .unwrap_or(self.default_format);

        // Parse the edit operation based on format
        let op = match format {
            EditFormat::Diff => {
                let content = args
                    .get("content")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| {
                        ToolError::InvalidArgs(
                            "missing required argument 'content' for diff format".into(),
                        )
                    })?;
                EditOp::Diff(content.to_string())
            }
            EditFormat::WholeFile => {
                let content = args
                    .get("content")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| {
                        ToolError::InvalidArgs(
                            "missing required argument 'content' for whole_file format".into(),
                        )
                    })?;
                EditOp::WholeFile(content.to_string())
            }
            EditFormat::StrReplace => {
                let old_str = args
                    .get("old_str")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| {
                        ToolError::InvalidArgs(
                            "missing required argument 'old_str' for str_replace format".into(),
                        )
                    })?;
                let new_str = args
                    .get("new_str")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| {
                        ToolError::InvalidArgs(
                            "missing required argument 'new_str' for str_replace format".into(),
                        )
                    })?;
                EditOp::StrReplace {
                    old_str: old_str.to_string(),
                    new_str: new_str.to_string(),
                }
            }
        };

        // Read existing file content for diff/str_replace
        let read_path = path.to_path_buf();
        let (original_content, text_format) =
            tokio::task::spawn_blocking(move || match fsio::read_existing(&read_path)? {
                Some(bytes) => {
                    fsio::check_unchanged(&read_path, &bytes)?;
                    fsio::decode(&read_path, bytes)
                }
                None => Ok((String::new(), fsio::TextFormat::plain())),
            })
            .await
            .map_err(|e| ToolError::ExecutionFailed(format!("read task failed: {e}")))??;

        // Apply the edit with preflight validation
        let new_content = match &op {
            EditOp::Diff(patch) => apply_diff_with_validation(&original_content, patch)?,
            EditOp::WholeFile(content) => content.clone(),
            EditOp::StrReplace { old_str, new_str } => {
                apply_str_replace(&original_content, old_str, new_str)?
            }
        };

        // Write the new content
        let bytes = text_format.restore(&new_content);
        let write_path = path.to_path_buf();
        tokio::task::spawn_blocking(move || {
            fsio::atomic_write(&write_path, &bytes)?;
            fsio::record(&write_path, &bytes);
            Ok::<_, std::io::Error>(())
        })
        .await
        .map_err(|e| ToolError::ExecutionFailed(format!("write task failed: {e}")))?
        .map_err(|e| {
            ToolError::ExecutionFailed(format!("failed to write '{}': {e}", path.display()))
        })?;

        let output = format!(
            "Edited {} ({} -> {} bytes)",
            path.display(),
            original_content.len(),
            new_content.len()
        );
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

/// Apply a unified diff/patch with preflight validation.
///
/// Validates that the patch can be applied cleanly before making any changes.
/// Returns the new content on success, or an error describing why the patch failed.
fn apply_diff_with_validation(original: &str, patch: &str) -> Result<String, ToolError> {
    // Parse the unified diff
    let diff = parse_unified_diff(patch)?;

    // Validate the diff can be applied (preflight)
    validate_diff(original, &diff)?;

    // Apply the diff
    apply_diff(original, &diff)
}

/// Parse a unified diff string into a list of hunks.
fn parse_unified_diff<'a>(patch: &'a str) -> Result<Vec<DiffHunk<'a>>, ToolError> {
    let lines: Vec<&str> = patch.lines().collect();
    let mut hunks = Vec::new();
    let mut i = 0;

    while i < lines.len() {
        let line = lines[i];
        if line.starts_with("--- ") || line.starts_with("+++ ") {
            // Skip file headers
            i += 1;
            continue;
        }
        if line.starts_with("@@ ") {
            // Parse hunk header: @@ -old_start,old_count +new_start,new_count @@
            let hunk = parse_hunk_header(line)?;
            i += 1;

            // Collect hunk lines until next hunk or end
            let mut hunk_lines = Vec::new();
            while i < lines.len() && !lines[i].starts_with("@@ ") {
                hunk_lines.push(lines[i]);
                i += 1;
            }

            hunks.push(DiffHunk {
                old_start: hunk.0,
                old_count: hunk.1,
                new_start: hunk.2,
                _new_count: hunk.3,
                lines: hunk_lines,
            });
        } else {
            i += 1;
        }
    }

    if hunks.is_empty() {
        return Err(ToolError::InvalidArgs(
            "no valid hunks found in diff".into(),
        ));
    }

    Ok(hunks)
}

/// A single hunk in a unified diff.
#[derive(Debug, Clone)]
#[allow(dead_code)] // new_start is parsed but not used in current implementation
struct DiffHunk<'a> {
    old_start: usize,
    old_count: usize,
    new_start: usize,
    _new_count: usize,
    lines: Vec<&'a str>,
}

/// Parse a hunk header line like `@@ -1,5 +1,6 @@`.
fn parse_hunk_header(line: &str) -> Result<(usize, usize, usize, usize), ToolError> {
    // Format: @@ -old_start,old_count +new_start,new_count @@
    let line = line.trim();
    if !line.starts_with("@@") || !line.ends_with("@@") {
        return Err(ToolError::InvalidArgs(format!(
            "invalid hunk header: {line}"
        )));
    }

    let inner = line[3..line.len() - 3].trim();
    let parts: Vec<&str> = inner.split_whitespace().collect();
    if parts.len() != 2 {
        return Err(ToolError::InvalidArgs(format!(
            "invalid hunk header format: {line}"
        )));
    }

    let parse_range = |s: &str| -> Result<(usize, usize), ToolError> {
        let s = s.trim_start_matches(['-', '+']);
        let parts: Vec<&str> = s.split(',').collect();
        let start = parts[0].parse::<usize>().map_err(|_| {
            ToolError::InvalidArgs(format!("invalid line number in hunk header: {s}"))
        })?;
        let count = if parts.len() > 1 {
            parts[1].parse::<usize>().map_err(|_| {
                ToolError::InvalidArgs(format!("invalid line count in hunk header: {s}"))
            })?
        } else {
            1
        };
        Ok((start, count))
    };

    let (old_start, old_count) = parse_range(parts[0])?;
    let (new_start, _new_count) = parse_range(parts[1])?;

    Ok((old_start, old_count, new_start, _new_count))
}

/// Validate that a diff can be applied to the original content.
fn validate_diff<'a>(original: &str, hunks: &[DiffHunk<'a>]) -> Result<(), ToolError> {
    let original_lines: Vec<&str> = original.lines().collect();

    for hunk in hunks {
        // Check bounds (1-indexed in diff, 0-indexed in vec)
        let start = hunk.old_start.saturating_sub(1);
        let end = start + hunk.old_count;

        if start > original_lines.len() {
            return Err(ToolError::InvalidArgs(format!(
                "hunk starts at line {} but file has only {} lines",
                hunk.old_start,
                original_lines.len()
            )));
        }

        if end > original_lines.len() {
            return Err(ToolError::InvalidArgs(format!(
                "hunk spans to line {} but file has only {} lines",
                end,
                original_lines.len()
            )));
        }

        // Verify context lines match
        // Track original line index separately from hunk line index
        // because additions ('+') don't correspond to original lines
        let mut orig_idx = start;
        for line in &hunk.lines {
            if line.starts_with(' ') || line.starts_with('-') {
                if orig_idx >= original_lines.len() {
                    return Err(ToolError::InvalidArgs(format!(
                        "hunk context exceeds file length at line {}",
                        orig_idx + 1
                    )));
                }
                let expected = &line[1..]; // Strip leading ' ' or '-'
                if original_lines[orig_idx] != expected {
                    return Err(ToolError::InvalidArgs(format!(
                        "context mismatch at line {}: expected {:?}, got {:?}",
                        orig_idx + 1,
                        expected,
                        original_lines[orig_idx]
                    )));
                }
                orig_idx += 1;
            }
            // '+' lines are additions - don't increment orig_idx
        }
    }

    Ok(())
}

/// Apply a validated diff to the original content.
fn apply_diff<'a>(original: &str, hunks: &[DiffHunk<'a>]) -> Result<String, ToolError> {
    // Preserve whether original ended with newline
    let had_trailing_newline = original.ends_with('\n');

    let mut original_lines: Vec<String> = original.lines().map(|s| s.to_string()).collect();
    let mut line_offset: isize = 0; // Track line shifts from previous hunks

    for hunk in hunks {
        let start = (hunk.old_start as isize - 1 + line_offset) as usize;
        let old_count = hunk.old_count;

        // Build new lines from hunk
        let mut new_lines = Vec::new();
        for line in &hunk.lines {
            if let Some(stripped) = line.strip_prefix('+') {
                // Added line
                new_lines.push(stripped.to_string());
            } else if let Some(stripped) = line.strip_prefix(' ') {
                // Context line - keep as is
                new_lines.push(stripped.to_string());
            }
            // '-' lines are deletions - skip them
        }

        // Replace the old lines with new lines
        let end = start + old_count;
        if end > original_lines.len() {
            return Err(ToolError::ExecutionFailed(
                "diff application failed: hunk bounds changed during application".into(),
            ));
        }

        original_lines.splice(start..end, new_lines.iter().cloned());

        // Update offset for subsequent hunks
        let new_count = hunk._new_count;
        line_offset += new_count as isize - old_count as isize;
    }

    let mut result = original_lines.join("\n");
    if had_trailing_newline {
        result.push('\n');
    }
    Ok(result)
}

/// Apply string replace (find old_str, replace with new_str).
fn apply_str_replace(original: &str, old_str: &str, new_str: &str) -> Result<String, ToolError> {
    if old_str.is_empty() {
        return Err(ToolError::InvalidArgs("old_str cannot be empty".into()));
    }

    if !original.contains(old_str) {
        return Err(ToolError::InvalidArgs(format!(
            "old_str not found in file: {:?}",
            old_str.chars().take(100).collect::<String>()
        )));
    }

    // Count occurrences to ensure unambiguous replacement
    let count = original.match_indices(old_str).count();
    if count > 1 {
        return Err(ToolError::InvalidArgs(format!(
            "old_str matches {} times in file; replacement would be ambiguous",
            count
        )));
    }

    Ok(original.replace(old_str, new_str))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;
    use tempfile::tempdir;

    #[tokio::test]
    async fn test_str_replace_preserves_crlf_and_bom() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("crlf.txt");
        fs::write(&p, "\u{feff}a\r\nb\r\nc\r\n").unwrap();
        EditTool::with_format(EditFormat::StrReplace)
            .execute(json!({"path": p.to_str().unwrap(), "old_str": "a\nb", "new_str": "a\nB"}))
            .await
            .unwrap();
        assert_eq!(fs::read(&p).unwrap(), "\u{feff}a\r\nB\r\nc\r\n".as_bytes());
    }

    #[tokio::test]
    async fn test_edit_refuses_non_utf8_and_stale_files() {
        let dir = tempdir().unwrap();
        let bin = dir.path().join("latin1.txt");
        fs::write(&bin, [b'a', 0xE9, b'\n']).unwrap();
        let tool = EditTool::with_format(EditFormat::StrReplace);
        let args = |p: &std::path::Path| json!({"path": p.to_str().unwrap(), "old_str": "a", "new_str": "b"});
        assert!(tool.execute(args(&bin)).await.is_err());

        let p = dir.path().join("stale.txt");
        fs::write(&p, "a\n").unwrap();
        fsio::record(&p, b"a\n");
        fs::write(&p, "a changed\n").unwrap();
        let err = tool.execute(args(&p)).await.unwrap_err().to_string();
        assert!(err.contains("changed since you last read"), "{err}");
    }

    #[tokio::test]
    async fn test_edit_tool_diff_basic() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("test.txt");
        fs::write(&file_path, "line 1\nline 2\nline 3\n").unwrap();

        let tool = EditTool::with_format(EditFormat::Diff);
        let patch = r#"@@ -1,3 +1,4 @@
 line 1
 line 2
+new line
 line 3
"#;
        let result = tool
            .execute(json!({
                "path": file_path.to_str().unwrap(),
                "format": "diff",
                "content": patch
            }))
            .await
            .unwrap();

        assert!(!result.truncated);
        let content = fs::read_to_string(&file_path).unwrap();
        assert_eq!(content, "line 1\nline 2\nnew line\nline 3\n");
    }

    #[tokio::test]
    async fn test_edit_tool_diff_delete() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("test.txt");
        fs::write(&file_path, "line 1\nline 2\nline 3\n").unwrap();

        let tool = EditTool::with_format(EditFormat::Diff);
        let patch = r#"@@ -1,3 +1,2 @@
 line 1
-line 2
 line 3
"#;
        let _result = tool
            .execute(json!({
                "path": file_path.to_str().unwrap(),
                "format": "diff",
                "content": patch
            }))
            .await
            .unwrap();

        let content = fs::read_to_string(&file_path).unwrap();
        assert_eq!(content, "line 1\nline 3\n");
    }

    #[tokio::test]
    async fn test_edit_tool_diff_replace() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("test.txt");
        fs::write(&file_path, "line 1\nline 2\nline 3\n").unwrap();

        let tool = EditTool::with_format(EditFormat::Diff);
        let patch = r#"@@ -1,3 +1,3 @@
 line 1
-line 2
+replaced line
 line 3
"#;
        let _result = tool
            .execute(json!({
                "path": file_path.to_str().unwrap(),
                "format": "diff",
                "content": patch
            }))
            .await
            .unwrap();

        let content = fs::read_to_string(&file_path).unwrap();
        assert_eq!(content, "line 1\nreplaced line\nline 3\n");
    }

    #[tokio::test]
    async fn test_edit_tool_diff_invalid_context_fails() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("test.txt");
        fs::write(&file_path, "line 1\nline 2\nline 3\n").unwrap();

        let tool = EditTool::with_format(EditFormat::Diff);
        // Wrong context - line 2 says "wrong" but file has "line 2"
        let patch = r#"@@ -1,3 +1,3 @@
 line 1
-wrong
 line 3
"#;
        let result = tool
            .execute(json!({
                "path": file_path.to_str().unwrap(),
                "format": "diff",
                "content": patch
            }))
            .await;

        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("context mismatch"));
    }

    #[tokio::test]
    async fn test_edit_tool_diff_out_of_bounds_fails() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("test.txt");
        fs::write(&file_path, "line 1\nline 2\n").unwrap();

        let tool = EditTool::with_format(EditFormat::Diff);
        // Hunk references line 5 but file only has 2 lines
        let patch = r#"@@ -5,1 +5,1 @@
-line 5
+new line
"#;
        let result = tool
            .execute(json!({
                "path": file_path.to_str().unwrap(),
                "format": "diff",
                "content": patch
            }))
            .await;

        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("spans to line") || err.contains("starts at line"));
    }

    #[tokio::test]
    async fn test_edit_tool_whole_file() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("test.txt");
        fs::write(&file_path, "old content\n").unwrap();

        let tool = EditTool::with_format(EditFormat::WholeFile);
        let _result = tool
            .execute(json!({
                "path": file_path.to_str().unwrap(),
                "format": "whole_file",
                "content": "new content\n"
            }))
            .await
            .unwrap();

        let content = fs::read_to_string(&file_path).unwrap();
        assert_eq!(content, "new content\n");
    }

    #[tokio::test]
    async fn test_edit_tool_whole_file_create_new() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("new_file.txt");

        let tool = EditTool::with_format(EditFormat::WholeFile);
        let _result = tool
            .execute(json!({
                "path": file_path.to_str().unwrap(),
                "format": "whole_file",
                "content": "brand new file\n"
            }))
            .await
            .unwrap();

        let content = fs::read_to_string(&file_path).unwrap();
        assert_eq!(content, "brand new file\n");
    }

    #[tokio::test]
    async fn test_edit_tool_str_replace() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("test.txt");
        fs::write(&file_path, "hello world\nhello universe\n").unwrap();

        let tool = EditTool::with_format(EditFormat::StrReplace);
        let _result = tool
            .execute(json!({
                "path": file_path.to_str().unwrap(),
                "format": "str_replace",
                "old_str": "hello world",
                "new_str": "hi world"
            }))
            .await
            .unwrap();

        let content = fs::read_to_string(&file_path).unwrap();
        assert_eq!(content, "hi world\nhello universe\n");
    }

    #[tokio::test]
    async fn test_edit_tool_str_replace_not_found_fails() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("test.txt");
        fs::write(&file_path, "hello world\n").unwrap();

        let tool = EditTool::with_format(EditFormat::StrReplace);
        let result = tool
            .execute(json!({
                "path": file_path.to_str().unwrap(),
                "format": "str_replace",
                "old_str": "not in file",
                "new_str": "replacement"
            }))
            .await;

        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("not found"));
    }

    #[tokio::test]
    async fn test_edit_tool_str_replace_ambiguous_fails() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("test.txt");
        fs::write(&file_path, "hello\nhello\nhello\n").unwrap();

        let tool = EditTool::with_format(EditFormat::StrReplace);
        let result = tool
            .execute(json!({
                "path": file_path.to_str().unwrap(),
                "format": "str_replace",
                "old_str": "hello",
                "new_str": "hi"
            }))
            .await;

        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("ambiguous"));
    }

    #[tokio::test]
    async fn test_edit_tool_str_replace_empty_old_str_fails() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("test.txt");
        fs::write(&file_path, "hello\n").unwrap();

        let tool = EditTool::with_format(EditFormat::StrReplace);
        let result = tool
            .execute(json!({
                "path": file_path.to_str().unwrap(),
                "format": "str_replace",
                "old_str": "",
                "new_str": "hi"
            }))
            .await;

        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("empty"));
    }

    #[tokio::test]
    async fn test_edit_tool_missing_args() {
        let tool = EditTool::new();
        let result = tool.execute(json!({})).await;
        assert!(result.is_err());

        let result = tool.execute(json!({"path": "/tmp/x"})).await;
        assert!(result.is_err()); // Missing content/old_str+new_str

        let result = tool
            .execute(json!({"path": "/tmp/x", "format": "diff"}))
            .await;
        assert!(result.is_err()); // Missing content for diff
    }

    #[tokio::test]
    async fn test_edit_tool_diff_multiple_hunks() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("test.txt");
        fs::write(&file_path, "line 1\nline 2\nline 3\nline 4\nline 5\n").unwrap();

        let tool = EditTool::with_format(EditFormat::Diff);
        let patch = r#"@@ -1,2 +1,3 @@
 line 1
+new at top
 line 2
@@ -4,2 +5,2 @@
 line 4
-line 5
+replaced 5
"#;
        let _result = tool
            .execute(json!({
                "path": file_path.to_str().unwrap(),
                "format": "diff",
                "content": patch
            }))
            .await
            .unwrap();

        let content = fs::read_to_string(&file_path).unwrap();
        assert_eq!(
            content,
            "line 1\nnew at top\nline 2\nline 3\nline 4\nreplaced 5\n"
        );
    }

    #[tokio::test]
    async fn test_edit_format_default_for_tier() {
        assert_eq!(
            EditFormat::default_for_tier(crate::model::ModelTier::Frontier),
            EditFormat::Diff
        );
        assert_eq!(
            EditFormat::default_for_tier(crate::model::ModelTier::Mid),
            EditFormat::Diff
        );
        assert_eq!(
            EditFormat::default_for_tier(crate::model::ModelTier::Small),
            EditFormat::WholeFile
        );
    }

    #[tokio::test]
    async fn test_edit_format_from_str() {
        assert_eq!("diff".parse::<EditFormat>(), Ok(EditFormat::Diff));
        assert_eq!(
            "whole_file".parse::<EditFormat>(),
            Ok(EditFormat::WholeFile)
        );
        assert_eq!(
            "str_replace".parse::<EditFormat>(),
            Ok(EditFormat::StrReplace)
        );
        assert!("invalid".parse::<EditFormat>().is_err());
    }
}
