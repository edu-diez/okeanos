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
            crate::model::ModelTier::Small => EditFormat::StrReplace,
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
    /// Advertise only the fields of `default_format` (one format, no `oneOf`).
    simple_schema: bool,
}

impl EditTool {
    pub fn new() -> Self {
        Self {
            default_format: EditFormat::Diff,
            simple_schema: false,
        }
    }

    /// Create an edit tool with a specific default format.
    pub fn with_format(format: EditFormat) -> Self {
        Self {
            default_format: format,
            simple_schema: false,
        }
    }

    /// Advertise a minimal schema for `format` (for weaker models): only that format's
    /// fields, no `format` selector, no `oneOf`. Other formats are still accepted.
    pub fn simplified(mut self) -> Self {
        self.simple_schema = true;
        self
    }

    fn simple_spec(&self) -> ToolSpec {
        let (description, properties, required) = match self.default_format {
            EditFormat::StrReplace => (
                "Replace text in a file. `old_str` must match one place in the file; copy it \
                 exactly from the file (include a few surrounding lines if it is not unique).",
                serde_json::json!({
                    "path": {"type": "string", "description": "Path to the file."},
                    "old_str": {"type": "string", "description": "Exact text to replace."},
                    "new_str": {"type": "string", "description": "Replacement text."}
                }),
                serde_json::json!(["path", "old_str", "new_str"]),
            ),
            EditFormat::WholeFile => (
                "Overwrite a file with new content. `content` is the complete new file.",
                serde_json::json!({
                    "path": {"type": "string", "description": "Path to the file."},
                    "content": {"type": "string", "description": "Complete new file content."}
                }),
                serde_json::json!(["path", "content"]),
            ),
            EditFormat::Diff => (
                "Edit a file with a unified diff. `content` is the diff with @@ hunk headers.",
                serde_json::json!({
                    "path": {"type": "string", "description": "Path to the file."},
                    "content": {"type": "string", "description": "Unified diff."}
                }),
                serde_json::json!(["path", "content"]),
            ),
        };
        ToolSpec::new(
            "edit",
            description,
            serde_json::json!({
                "type": "object",
                "properties": properties,
                "required": required
            }),
        )
        .with_category(crate::tool::tool_trait::ToolCategory::Write)
    }
}

#[async_trait]
impl Tool for EditTool {
    fn spec(&self) -> ToolSpec {
        if self.simple_schema {
            return self.simple_spec();
        }
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
        let mut fuzzy = false;
        let new_content = match &op {
            EditOp::Diff(patch) => apply_diff_with_validation(&original_content, patch)?,
            EditOp::WholeFile(content) => content.clone(),
            EditOp::StrReplace { old_str, new_str } => {
                let (content, used_fuzzy) = replace_forgiving(&original_content, old_str, new_str)?;
                fuzzy = used_fuzzy;
                content
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

        let mut output = format!(
            "Edited {} ({} -> {} bytes)",
            path.display(),
            original_content.len(),
            new_content.len()
        );
        if fuzzy {
            output.push_str("; old_str matched ignoring whitespace differences");
        }
        let (output, truncated) = truncate_output(&output, MAX_OUTPUT_BYTES);

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
/// Replace `old_str` with `new_str`, tolerating whitespace differences.
///
/// 1. Exact unique match.
/// 2. Otherwise a unique line-wise match ignoring leading/trailing whitespace
///    (the replacement is re-indented to the file's indentation).
///
/// Returns the new content and whether the forgiving match was used. On failure the
/// error shows where the match is ambiguous or the closest region of the file.
fn replace_forgiving(
    original: &str,
    old_str: &str,
    new_str: &str,
) -> Result<(String, bool), ToolError> {
    if old_str.is_empty() {
        return Err(ToolError::InvalidArgs("old_str cannot be empty".into()));
    }

    let exact: Vec<usize> = original.match_indices(old_str).map(|(i, _)| i).collect();
    match exact.len() {
        1 => return Ok((original.replacen(old_str, new_str, 1), false)),
        n if n > 1 => {
            let lines: Vec<String> = exact
                .iter()
                .map(|&i| (original[..i].matches('\n').count() + 1).to_string())
                .collect();
            return Err(ToolError::InvalidArgs(format!(
                "old_str matches {n} times (at lines {}); replacement would be ambiguous. \
                 Include more surrounding lines in old_str to make it unique.",
                lines.join(", ")
            )));
        }
        _ => {}
    }

    let file_lines: Vec<&str> = original.split_inclusive('\n').collect();
    let old_lines: Vec<&str> = old_str.trim_matches('\n').lines().collect();
    let m = old_lines.len();
    if m == 0 || m > file_lines.len() {
        return Err(not_found_error(&file_lines, &old_lines));
    }

    let matches: Vec<usize> = (0..=file_lines.len() - m)
        .filter(|&s| (0..m).all(|k| file_lines[s + k].trim() == old_lines[k].trim()))
        .collect();
    match matches.len() {
        0 => Err(not_found_error(&file_lines, &old_lines)),
        1 => {
            let s = matches[0];
            let file_indent = leading_ws(file_lines[s]);
            let old_indent = leading_ws(old_lines[0]);
            let mut replacement = reindent(new_str.trim_matches('\n'), old_indent, file_indent);
            let last_had_newline = file_lines[s + m - 1].ends_with('\n');
            if !replacement.is_empty() && last_had_newline {
                replacement.push('\n');
            }
            let mut out = String::with_capacity(original.len() + replacement.len());
            out.extend(file_lines[..s].iter().copied());
            out.push_str(&replacement);
            out.extend(file_lines[s + m..].iter().copied());
            Ok((out, true))
        }
        n => {
            let at: Vec<String> = matches.iter().map(|s| (s + 1).to_string()).collect();
            Err(ToolError::InvalidArgs(format!(
                "old_str matches {n} places ignoring whitespace (starting at lines {}); \
                 replacement would be ambiguous. Include more surrounding lines.",
                at.join(", ")
            )))
        }
    }
}

fn leading_ws(line: &str) -> &str {
    &line[..line.len() - line.trim_start().len()]
}

/// Swap the `from` indentation prefix for `to` on every line that has it.
fn reindent(text: &str, from: &str, to: &str) -> String {
    if from == to {
        return text.to_string();
    }
    text.lines()
        .map(|l| match l.strip_prefix(from) {
            Some(rest) => format!("{to}{rest}"),
            None => l.to_string(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Error for a failed match, showing the file region most similar to `old_lines`.
fn not_found_error(file_lines: &[&str], old_lines: &[&str]) -> ToolError {
    let mut msg = String::from("old_str not found in file.");
    let m = old_lines.len();
    if m > 0 && m <= file_lines.len() {
        let mut best: Option<(f32, usize)> = None;
        for s in 0..=file_lines.len() - m {
            let score: f32 = (0..m)
                .map(|k| line_similarity(file_lines[s + k], old_lines[k]))
                .sum();
            if best.is_none_or(|(b, _)| score > b) {
                best = Some((score, s));
            }
        }
        if let Some((score, s)) = best
            && score / m as f32 >= 0.4
        {
            let end = (s + m).min(file_lines.len());
            msg.push_str(&format!(
                " Closest region in the file (lines {}-{}); copy old_str from it exactly:\n",
                s + 1,
                end
            ));
            for (i, l) in file_lines[s..end].iter().take(15).enumerate() {
                msg.push_str(&format!("{}: {}\n", s + i + 1, l.trim_end_matches('\n')));
            }
            return ToolError::InvalidArgs(msg.trim_end().to_string());
        }
    }
    msg.push_str(" Read the file again and copy the exact text you want to replace.");
    ToolError::InvalidArgs(msg)
}

/// Jaccard similarity of whitespace-separated tokens (1.0 when equal ignoring whitespace).
fn line_similarity(a: &str, b: &str) -> f32 {
    let (a, b) = (a.trim(), b.trim());
    if a == b {
        return 1.0;
    }
    let ta: std::collections::HashSet<&str> = a.split_whitespace().collect();
    let tb: std::collections::HashSet<&str> = b.split_whitespace().collect();
    let union = ta.union(&tb).count();
    if union == 0 {
        return 0.0;
    }
    ta.intersection(&tb).count() as f32 / union as f32
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
            EditFormat::StrReplace
        );
    }

    #[test]
    fn str_replace_matches_ignoring_whitespace_and_reindents() {
        let file = "fn a() {\n    let x = 1;\n    let y = 2;\n}\n";
        // Model dropped the indentation and added trailing spaces.
        let (out, fuzzy) =
            replace_forgiving(file, "let x = 1;  \nlet y = 2;", "let x = 10;\nlet y = 20;")
                .unwrap();
        assert!(fuzzy);
        assert_eq!(out, "fn a() {\n    let x = 10;\n    let y = 20;\n}\n");
    }

    #[test]
    fn str_replace_ambiguous_lists_lines() {
        let err = replace_forgiving("a\nb\na\n", "a", "c")
            .unwrap_err()
            .to_string();
        assert!(err.contains("lines 1, 3"), "{err}");
        let err = replace_forgiving("  a\nb\n a\n", "a ", "c")
            .unwrap_err()
            .to_string();
        assert!(err.contains("ambiguous"), "{err}");
    }

    #[test]
    fn str_replace_not_found_shows_closest_region() {
        let file = "one\nfn compute(total: u32) -> u32 {\n    total + 1\n}\n";
        let err = replace_forgiving(file, "fn compute(count: u32) -> u32 {", "x")
            .unwrap_err()
            .to_string();
        assert!(err.contains("Closest region"), "{err}");
        assert!(err.contains("2: fn compute(total: u32)"), "{err}");
        let err = replace_forgiving(file, "zzz qqq", "x")
            .unwrap_err()
            .to_string();
        assert!(!err.contains("Closest region"), "{err}");
    }

    #[test]
    fn simplified_schema_has_one_format_and_no_one_of() {
        let spec = EditTool::with_format(EditFormat::StrReplace)
            .simplified()
            .spec();
        let props = spec.input_schema["properties"].as_object().unwrap();
        assert!(props.contains_key("old_str") && !props.contains_key("format"));
        assert!(spec.input_schema.get("oneOf").is_none());
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
