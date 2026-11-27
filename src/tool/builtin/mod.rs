//! Built-in tools: read, write, delete, run, grep, glob.
//!
//! All built-in tools execute under the permission layer.
//! Tool output is truncated to a maximum byte limit with head/tail
//! overflow indication; truncated output counts against the context cap.

pub mod delete;
pub mod edit;
pub mod fsio;
pub mod glob;
pub mod grep;
pub mod read;
pub mod remember;
pub mod run;
pub mod skill;
pub mod tool_search;
pub mod write;

pub use delete::DeleteTool;
pub use edit::{EditFormat, EditTool};
pub use glob::GlobTool;
pub use grep::GrepTool;
pub use read::ReadTool;
pub use remember::RememberTool;
pub use run::RunTool;
pub use skill::SkillTool;
pub use tool_search::ToolSearchTool;
pub use write::WriteTool;

/// Maximum output bytes per tool result before truncation.
pub const MAX_OUTPUT_BYTES: usize = 10_000;

/// Truncate `output` to at most `max_bytes` using head/tail with an
/// overflow indicator. Returns the (possibly truncated) string and a
/// flag indicating whether truncation occurred.
pub fn truncate_output(output: &str, max_bytes: usize) -> (String, bool) {
    if output.len() <= max_bytes {
        return (output.to_string(), false);
    }

    let half = max_bytes / 2;

    // Find the nearest UTF-8 char boundary at or before `half` bytes.
    let head_end = output
        .char_indices()
        .take_while(|(i, _)| *i <= half)
        .last()
        .map(|(i, _)| i)
        .unwrap_or(0);

    // Find the nearest UTF-8 char boundary at or after `len - half` bytes.
    let tail_start_candidate = output.len().saturating_sub(half);
    let tail_start = output
        .char_indices()
        .find(|(i, _)| *i >= tail_start_candidate)
        .map(|(i, _)| i)
        .unwrap_or(output.len());

    let head = &output[..head_end];
    let tail = &output[tail_start..];
    let omitted = output.len() - head.len() - tail.len();

    (
        format!(
            "{}\n...[output truncated: {} bytes omitted]...\n{}",
            head, omitted, tail
        ),
        true,
    )
}

/// Like [`truncate_output`], but when truncation occurs the full output is
/// saved to a temp file and its path is appended so the model can `grep` it
/// instead of re-running the command. Spill failures are silently ignored.
pub fn truncate_output_with_spill(output: &str, max_bytes: usize) -> (String, bool) {
    let (mut text, truncated) = truncate_output(output, max_bytes);
    if truncated && let Some(path) = spill_output(output) {
        text.push_str(&format!(
            "\n[full output saved to {}; use grep on it to search]",
            path.display()
        ));
    }
    (text, truncated)
}

fn spill_output(output: &str) -> Option<std::path::PathBuf> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let dir = std::env::temp_dir().join("okeanos").join("tool-output");
    std::fs::create_dir_all(&dir).ok()?;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = dir.join(format!("{}-{nanos}-{n}.txt", std::process::id()));
    std::fs::write(&path, output).ok()?;
    Some(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_spill_saves_full_output_and_notes_path() {
        let input = "x".repeat(500);
        let (result, truncated) = truncate_output_with_spill(&input, 100);
        assert!(truncated);
        let path = result
            .split("saved to ")
            .nth(1)
            .and_then(|s| s.split(';').next())
            .unwrap();
        assert_eq!(std::fs::read_to_string(path).unwrap(), input);
        let _ = std::fs::remove_file(path);

        let (small, truncated) = truncate_output_with_spill("hi", 100);
        assert!(!truncated);
        assert_eq!(small, "hi");
    }

    #[test]
    fn test_truncate_output_under_limit() {
        let input = "hello world";
        let (result, truncated) = truncate_output(input, 100);
        assert!(!truncated);
        assert_eq!(result, input);
    }

    #[test]
    fn test_truncate_output_over_limit() {
        let input = "a".repeat(5000);
        let (result, truncated) = truncate_output(input.as_str(), 100);
        assert!(truncated);
        assert!(result.contains("[output truncated"));
        assert!(result.len() <= 200); // head + indicator + tail + overhead
    }

    #[test]
    fn test_truncate_output_exact_limit() {
        let input = "a".repeat(100);
        let (result, truncated) = truncate_output(input.as_str(), 100);
        assert!(!truncated);
        assert_eq!(result.len(), 100);
    }

    #[test]
    fn test_truncate_output_utf8_safe() {
        let input = "héllo wörld 🦀".repeat(500);
        let (result, truncated) = truncate_output(input.as_str(), 50);
        assert!(truncated);
        // Should not panic on UTF-8 boundaries
        assert!(result.is_char_boundary(0));
        assert!(result.is_char_boundary(result.len()));
    }
}
