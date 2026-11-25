//! Built-in `remember` tool — save a durable note for future sessions.
//!
//! Category: Write (requires approval). Writes `<memory dir>/<slug>.md` and
//! upserts one line in `<memory dir>/MEMORY.md`. Names are slugged so a write
//! can never leave the memory directory. Takes effect from the next session.

use crate::context::memory::MEMORY_INDEX_FILE;
use crate::tool::tool_trait::{Tool, ToolCategory, ToolError, ToolExecutionResult, ToolSpec};
use async_trait::async_trait;
use serde_json::Value;
use std::path::PathBuf;

const MAX_SLUG_LEN: usize = 64;
const MAX_DESCRIPTION_CHARS: usize = 200;
const MAX_CONTENT_BYTES: usize = 16 * 1024;

/// Built-in tool that persists notes across sessions.
#[derive(Debug)]
pub struct RememberTool {
    project_dir: PathBuf,
    user_dir: Option<PathBuf>,
}

impl RememberTool {
    pub fn new(project_dir: PathBuf, user_dir: Option<PathBuf>) -> Self {
        Self {
            project_dir,
            user_dir,
        }
    }
}

/// Reduce a free-form name to `[a-z0-9_-]`, at most 64 chars.
fn slugify(name: &str) -> String {
    let mut slug = String::new();
    for c in name.trim().chars() {
        let c = c.to_ascii_lowercase();
        if c.is_ascii_alphanumeric() || c == '_' {
            slug.push(c);
        } else if !slug.ends_with('-') {
            slug.push('-');
        }
    }
    let slug = slug.trim_matches('-');
    slug.chars().take(MAX_SLUG_LEN).collect::<String>()
}

fn upsert_index(existing: &str, slug: &str, line: &str) -> String {
    let marker = format!("]({slug}.md)");
    let mut replaced = false;
    let mut lines: Vec<String> = existing
        .lines()
        .map(|l| {
            if l.starts_with("- [") && l.contains(&marker) {
                replaced = true;
                line.to_string()
            } else {
                l.to_string()
            }
        })
        .collect();
    if !replaced {
        lines.push(line.to_string());
    }
    let mut out = lines.join("\n");
    out.push('\n');
    out
}

#[async_trait]
impl Tool for RememberTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "remember",
            "Save a durable note that future sessions will see in their memory index. Use for \
             lasting preferences, conventions, and decisions — not for task-specific details. \
             Saving under an existing name replaces that note.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "scope": {
                        "type": "string",
                        "enum": ["project", "user"],
                        "description": "'project' (this repo, default) or 'user' (all projects)."
                    },
                    "name": {
                        "type": "string",
                        "description": "Short topic name (becomes the file name)."
                    },
                    "description": {
                        "type": "string",
                        "description": "One-line summary shown in the memory index."
                    },
                    "content": {
                        "type": "string",
                        "description": "The note itself, in markdown."
                    }
                },
                "required": ["name", "description", "content"]
            }),
        )
        .with_category(ToolCategory::Write)
    }

    async fn execute(&self, args: Value) -> Result<ToolExecutionResult, ToolError> {
        let get = |key: &str| {
            args.get(key)
                .and_then(|v| v.as_str())
                .ok_or_else(|| ToolError::InvalidArgs(format!("missing required argument '{key}'")))
        };
        let name = get("name")?;
        let description = get("description")?;
        let content = get("content")?;

        let dir = match args
            .get("scope")
            .and_then(|v| v.as_str())
            .unwrap_or("project")
        {
            "project" => self.project_dir.clone(),
            "user" => self.user_dir.clone().ok_or_else(|| {
                ToolError::ExecutionFailed(
                    "user memory directory unavailable (set HOME or XDG_CONFIG_HOME)".into(),
                )
            })?,
            other => {
                return Err(ToolError::InvalidArgs(format!(
                    "invalid scope '{other}' (expected 'project' or 'user')"
                )));
            }
        };

        let slug = slugify(name);
        // "memory" would collide with MEMORY.md on case-insensitive filesystems.
        if slug.is_empty() || slug == "memory" {
            return Err(ToolError::InvalidArgs(format!(
                "name '{name}' is not usable; choose a descriptive topic name"
            )));
        }
        if content.len() > MAX_CONTENT_BYTES {
            return Err(ToolError::InvalidArgs(format!(
                "content is {} bytes; the limit is {MAX_CONTENT_BYTES}. Split it into several notes.",
                content.len()
            )));
        }
        let description: String = description
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .chars()
            .take(MAX_DESCRIPTION_CHARS)
            .collect();

        std::fs::create_dir_all(&dir).map_err(|e| {
            ToolError::ExecutionFailed(format!("failed to create '{}': {e}", dir.display()))
        })?;

        let topic_path = dir.join(format!("{slug}.md"));
        std::fs::write(
            &topic_path,
            format!("# {}\n\n{}\n", name.trim(), content.trim_end()),
        )
        .map_err(|e| {
            ToolError::ExecutionFailed(format!("failed to write '{}': {e}", topic_path.display()))
        })?;

        let index_path = dir.join(MEMORY_INDEX_FILE);
        let existing = std::fs::read_to_string(&index_path).unwrap_or_default();
        let line = format!("- [{}]({slug}.md) — {description}", name.trim());
        std::fs::write(&index_path, upsert_index(&existing, &slug, &line)).map_err(|e| {
            ToolError::ExecutionFailed(format!("failed to update '{}': {e}", index_path.display()))
        })?;

        Ok(ToolExecutionResult {
            output: format!(
                "Saved memory '{slug}' to {} (available from the next session).",
                topic_path.display()
            ),
            exit_code: None,
            timed_out: false,
            cancelled: false,
            truncated: false,
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
    use tempfile::tempdir;

    fn tool(tmp: &std::path::Path) -> RememberTool {
        RememberTool::new(tmp.join("project"), Some(tmp.join("user")))
    }

    #[test]
    fn slugify_is_safe() {
        assert_eq!(slugify("../../etc/passwd"), "etc-passwd");
        assert_eq!(slugify("  Build & Test!  "), "build-test");
        assert_eq!(slugify("..."), "");
        assert_eq!(slugify(&"a".repeat(200)).len(), MAX_SLUG_LEN);
    }

    #[tokio::test]
    async fn saves_topic_and_index_then_upserts() {
        let tmp = tempdir().unwrap();
        let t = tool(tmp.path());
        t.execute(
            json!({"name": "Test cmd", "description": "how to test", "content": "cargo test"}),
        )
        .await
        .unwrap();
        t.execute(json!({"name": "Style", "description": "fmt", "content": "rustfmt"}))
            .await
            .unwrap();
        t.execute(
            json!({"name": "Test cmd", "description": "updated", "content": "cargo nextest"}),
        )
        .await
        .unwrap();

        let dir = tmp.path().join("project");
        let index = std::fs::read_to_string(dir.join("MEMORY.md")).unwrap();
        assert_eq!(index.lines().count(), 2);
        assert!(index.contains("- [Test cmd](test-cmd.md) — updated"));
        let topic = std::fs::read_to_string(dir.join("test-cmd.md")).unwrap();
        assert!(topic.contains("cargo nextest") && !topic.contains("cargo test"));
    }

    #[tokio::test]
    async fn user_scope_and_traversal_and_reserved_names() {
        let tmp = tempdir().unwrap();
        let t = tool(tmp.path());
        t.execute(
            json!({"scope": "user", "name": "../escape", "description": "d", "content": "c"}),
        )
        .await
        .unwrap();
        assert!(tmp.path().join("user").join("escape.md").is_file());
        assert!(!tmp.path().join("escape.md").exists());

        for bad in ["memory", "***"] {
            let err = t
                .execute(json!({"name": bad, "description": "d", "content": "c"}))
                .await;
            assert!(matches!(err, Err(ToolError::InvalidArgs(_))), "{bad}");
        }
        let err = t
            .execute(json!({"scope": "other", "name": "x", "description": "d", "content": "c"}))
            .await;
        assert!(matches!(err, Err(ToolError::InvalidArgs(_))));
    }

    #[test]
    fn is_write_category() {
        let tmp = tempdir().unwrap();
        assert_eq!(tool(tmp.path()).spec().category, ToolCategory::Write);
    }
}
