//! Built-in tools: read, write, delete, run, grep.
//!
//! All built-in tools execute under the permission layer.
//! Tool output is truncated to a maximum byte limit with head/tail
//! overflow indication; truncated output counts against the context cap.

pub mod delete;
pub mod grep;
pub mod read;
pub mod run;
pub mod write;

pub use delete::DeleteTool;
pub use grep::GrepTool;
pub use read::ReadTool;
pub use run::RunTool;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_truncate_output_under_limit() {
        let input = "hello world";
        let (result, truncated) = truncate_output(&input, 100);
        assert!(!truncated);
        assert_eq!(result, input);
    }

    #[test]
    fn test_truncate_output_over_limit() {
        let input = "a".repeat(5000);
        let (result, truncated) = truncate_output(&input, 100);
        assert!(truncated);
        assert!(result.contains("[output truncated"));
        assert!(result.len() <= 200); // head + indicator + tail + overhead
    }

    #[test]
    fn test_truncate_output_exact_limit() {
        let input = "a".repeat(100);
        let (result, truncated) = truncate_output(&input, 100);
        assert!(!truncated);
        assert_eq!(result.len(), 100);
    }

    #[test]
    fn test_truncate_output_utf8_safe() {
        let input = "héllo wörld 🦀".repeat(500);
        let (result, truncated) = truncate_output(&input, 50);
        assert!(truncated);
        // Should not panic on UTF-8 boundaries
        assert!(result.is_char_boundary(0));
        assert!(result.is_char_boundary(result.len()));
    }
}
