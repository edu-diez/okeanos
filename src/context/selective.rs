//! Selective file inclusion using pure Rust (no C dependencies).
//!
//! Finds task-relevant files by scanning for symbols/patterns in the codebase.
//! Uses standard library file walking and regex for pattern matching.

use crate::context::tokens::TokenCounter;
use regex::Regex;
use std::path::{Path, PathBuf};
use thiserror::Error;

/// Configuration for selective context inclusion.
#[derive(Debug, Clone)]
pub struct SelectiveContextConfig {
    /// Maximum number of files to include.
    pub max_files: usize,
    /// Maximum tokens per included file (truncated if exceeded).
    pub max_tokens_per_file: usize,
    /// File patterns to include (e.g., "*.rs", "*.ts").
    pub include_patterns: Vec<String>,
    /// File patterns to exclude (e.g., "target/", "node_modules/").
    pub exclude_patterns: Vec<String>,
    /// Whether to follow symlinks.
    pub follow_symlinks: bool,
    /// Scan the project for files matching the prompt's terms. Off by default:
    /// it is slow on large trees and guesses; explicit `@path` mentions are always expanded.
    pub auto_scan: bool,
}

impl Default for SelectiveContextConfig {
    fn default() -> Self {
        Self {
            auto_scan: false,
            max_files: 10,
            max_tokens_per_file: 2000,
            include_patterns: vec![
                "*.rs".to_string(),
                "*.ts".to_string(),
                "*.js".to_string(),
                "*.py".to_string(),
            ],
            exclude_patterns: vec![
                "target/".to_string(),
                "node_modules/".to_string(),
                ".git/".to_string(),
                "dist/".to_string(),
                "build/".to_string(),
                "*.lock".to_string(),
            ],
            follow_symlinks: false,
        }
    }
}

/// A file included in context with its relevance score.
#[derive(Debug, Clone)]
pub struct IncludedFile {
    /// Relative path from project root.
    pub path: PathBuf,
    /// File content (truncated to max_tokens_per_file).
    pub content: String,
    /// Estimated token count.
    pub tokens: usize,
    /// Relevance score (higher = more relevant).
    pub score: f64,
    /// Why this file was included (matched symbols/patterns).
    pub match_reasons: Vec<String>,
}

/// Errors from selective context scanning.
#[derive(Debug, Error)]
pub enum SelectiveContextError {
    #[error("regex error: {0}")]
    Regex(#[from] regex::Error),
    #[error("I/O error reading {path}: {source}")]
    IoRead {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("project root not found")]
    NoProjectRoot,
}

/// Extract symbols/identifiers from a task description for searching.
pub fn extract_search_terms(task: &str) -> Vec<String> {
    let mut terms = Vec::new();

    // Extract function/method names (word followed by parens or ::)
    let func_regex = Regex::new(r"\b([a-zA-Z_][a-zA-Z0-9_]*)\s*\(").unwrap();
    for cap in func_regex.captures_iter(task) {
        if let Some(m) = cap.get(1) {
            terms.push(m.as_str().to_string());
        }
    }

    // Extract type/struct names (capitalized words)
    let type_regex = Regex::new(r"\b([A-Z][a-zA-Z0-9_]*)\b").unwrap();
    for cap in type_regex.captures_iter(task) {
        if let Some(m) = cap.get(1) {
            terms.push(m.as_str().to_string());
        }
    }

    // Extract snake_case identifiers
    let snake_regex = Regex::new(r"\b([a-z][a-z0-9_]*[a-z0-9])\b").unwrap();
    for cap in snake_regex.captures_iter(task) {
        if let Some(m) = cap.get(1) {
            let term = m.as_str();
            if term.len() > 2 && !is_common_word(term) {
                terms.push(term.to_string());
            }
        }
    }

    // Extract kebab-case identifiers
    let kebab_regex = Regex::new(r"\b([a-z][a-z0-9-]*[a-z0-9])\b").unwrap();
    for cap in kebab_regex.captures_iter(task) {
        if let Some(m) = cap.get(1) {
            let term = m.as_str();
            if term.len() > 2 && !is_common_word(term) {
                terms.push(term.to_string());
            }
        }
    }

    // Deduplicate and filter
    terms.sort();
    terms.dedup();
    terms.into_iter().filter(|t| t.len() > 2).take(20).collect()
}

fn is_common_word(word: &str) -> bool {
    matches!(
        word,
        "the"
            | "and"
            | "for"
            | "with"
            | "from"
            | "that"
            | "this"
            | "have"
            | "will"
            | "been"
            | "their"
            | "there"
            | "would"
            | "could"
            | "should"
            | "about"
            | "after"
            | "before"
            | "while"
            | "during"
            | "through"
            | "between"
            | "under"
            | "over"
            | "above"
            | "below"
            | "into"
            | "onto"
            | "upon"
            | "within"
            | "without"
            | "across"
            | "against"
            | "among"
            | "around"
            | "behind"
            | "beneath"
            | "beside"
            | "beyond"
            | "despite"
            | "except"
            | "inside"
            | "outside"
            | "toward"
            | "towards"
            | "underneath"
            | "until"
            | "unless"
            | "since"
            | "once"
            | "when"
            | "where"
            | "whilst"
            | "whose"
            | "whom"
            | "which"
            | "who"
            | "what"
            | "why"
            | "how"
            | "then"
            | "than"
            | "now"
            | "just"
            | "only"
            | "also"
            | "even"
            | "still"
            | "yet"
            | "already"
            | "ever"
            | "never"
            | "always"
            | "often"
            | "sometimes"
            | "rarely"
            | "seldom"
            | "usually"
            | "generally"
            | "typically"
            | "normally"
            | "commonly"
            | "frequently"
            | "occasionally"
            | "hardly"
            | "scarcely"
            | "barely"
            | "merely"
            | "simply"
            | "really"
            | "truly"
            | "actually"
            | "certainly"
            | "definitely"
            | "probably"
            | "possibly"
            | "perhaps"
            | "maybe"
            | "likely"
            | "unlikely"
            | "surely"
            | "obviously"
            | "clearly"
            | "plainly"
            | "evidently"
    )
}

/// Find files relevant to a task using file walking and regex matching.
pub fn find_relevant_files(
    project_root: &Path,
    task: &str,
    config: &SelectiveContextConfig,
    token_counter: &TokenCounter,
) -> Result<Vec<IncludedFile>, SelectiveContextError> {
    let search_terms = extract_search_terms(task);
    if search_terms.is_empty() {
        return Ok(Vec::new());
    }

    // Build a regex pattern from search terms
    let pattern = search_terms.join("|");
    let matcher = Regex::new(&pattern)?;

    let mut file_scores: std::collections::HashMap<PathBuf, (f64, Vec<String>)> =
        std::collections::HashMap::new();

    // Walk the project directory (exclusions are applied while walking)
    walk_dir(
        project_root,
        project_root,
        &matcher,
        config,
        &mut file_scores,
    )?;

    // Deterministic ranking: score desc, then path
    let mut ranked: Vec<(PathBuf, f64, Vec<String>)> = file_scores
        .into_iter()
        .map(|(p, (s, r))| (p, s, r))
        .collect();
    ranked.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.cmp(&b.0))
    });

    // Only the top files are read a second time
    let mut results = Vec::new();
    for (path, score, reasons) in ranked {
        if results.len() >= config.max_files {
            break;
        }
        let rel_path = path.strip_prefix(project_root).unwrap_or(&path);
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        let truncated_content =
            truncate_to_tokens(&content, config.max_tokens_per_file, token_counter);
        let final_tokens = token_counter.count(&truncated_content);

        results.push(IncludedFile {
            path: rel_path.to_path_buf(),
            content: truncated_content,
            tokens: final_tokens,
            score,
            match_reasons: reasons.into_iter().take(5).collect(),
        });
    }

    Ok(results)
}

/// Extract `@path` mentions from a prompt (e.g. `look at @src/main.rs`).
pub fn extract_mentions(prompt: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for (i, _) in prompt.match_indices('@') {
        // Must start a token, so emails and `a@b` are ignored
        if prompt[..i]
            .chars()
            .next_back()
            .is_some_and(|c| !c.is_whitespace() && !"([\"'`".contains(c))
        {
            continue;
        }
        let rest = &prompt[i + 1..];
        let end = rest
            .find(|c: char| c.is_whitespace() || matches!(c, ')' | ']' | '"' | '\'' | '`' | ','))
            .unwrap_or(rest.len());
        let token = rest[..end].trim_end_matches(['.', ':', ';', '?', '!']);
        if token.is_empty() || !(token.contains('/') || token.contains('\\') || token.contains('.'))
        {
            continue;
        }
        if !out.iter().any(|t| t == token) {
            out.push(token.to_string());
        }
    }
    out
}

/// Read files explicitly mentioned with `@path` in the prompt.
///
/// Paths are resolved under `project_root`; anything that escapes it, is not a
/// regular file, or is not valid UTF-8 is skipped.
pub fn expand_mentions(
    project_root: &Path,
    prompt: &str,
    config: &SelectiveContextConfig,
    token_counter: &TokenCounter,
) -> Vec<IncludedFile> {
    let Ok(root) = project_root.canonicalize() else {
        return Vec::new();
    };
    let mut results = Vec::new();
    for mention in extract_mentions(prompt) {
        if results.len() >= config.max_files {
            break;
        }
        let Ok(full) = root.join(&mention).canonicalize() else {
            continue;
        };
        if !full.starts_with(&root) || !full.is_file() {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&full) else {
            continue;
        };
        let content = truncate_to_tokens(&content, config.max_tokens_per_file, token_counter);
        let tokens = token_counter.count(&content);
        results.push(IncludedFile {
            path: full.strip_prefix(&root).unwrap_or(&full).to_path_buf(),
            content,
            tokens,
            score: 1.0,
            match_reasons: vec![format!("mentioned: @{mention}")],
        });
    }
    results
}

/// Recursively walk directory and score files.
fn walk_dir(
    root: &Path,
    current: &Path,
    matcher: &Regex,
    config: &SelectiveContextConfig,
    scores: &mut std::collections::HashMap<PathBuf, (f64, Vec<String>)>,
) -> Result<(), SelectiveContextError> {
    let entries = std::fs::read_dir(current).map_err(|e| SelectiveContextError::IoRead {
        path: current.to_path_buf(),
        source: e,
    })?;

    for entry in entries {
        let entry = entry.map_err(|e| SelectiveContextError::IoRead {
            path: current.to_path_buf(),
            source: e,
        })?;

        let path = entry.path();
        let _file_name = entry.file_name().to_string_lossy().to_string();

        // Check exclude patterns early
        let rel_path = path.strip_prefix(root).unwrap_or(&path);
        let mut rel_str = rel_path.to_string_lossy().replace('\\', "/");
        let is_dir = path.is_dir();
        if is_dir {
            rel_str.push('/');
        }

        let excluded = config
            .exclude_patterns
            .iter()
            .any(|pat| glob_match(pat, &rel_str));

        if excluded {
            continue;
        }

        if is_dir {
            // Check if we should follow symlinks
            if !config.follow_symlinks && is_symlink(&path) {
                continue;
            }
            walk_dir(root, &path, matcher, config, scores)?;
        } else if path.is_file() {
            // Check include patterns
            let included = config
                .include_patterns
                .iter()
                .any(|pat| glob_match(pat, &rel_str));

            if !included {
                continue;
            }

            // Read and score file
            // Binary or unreadable files are simply not candidates
            let Ok(content) = std::fs::read_to_string(&path) else {
                continue;
            };

            let mut score = 0.0;
            let mut reasons = Vec::new();

            for line in content.lines() {
                if matcher.is_match(line) {
                    score += 1.0;
                    if reasons.len() < 5 {
                        reasons.push(format!("matched: {}", line.trim()));
                    }
                }
            }

            if score > 0.0 {
                scores.insert(path, (score, reasons));
            }
        }
    }

    Ok(())
}

/// Check if a path is a symlink.
fn is_symlink(path: &Path) -> bool {
    std::fs::read_link(path).is_ok()
}

/// Simple glob pattern matching.
fn glob_match(pattern: &str, text: &str) -> bool {
    if pattern.ends_with('/') {
        // Directory pattern
        text.starts_with(pattern) || text.contains(&format!("/{}", pattern))
    } else if pattern.starts_with("*.") {
        // Extension pattern
        text.ends_with(&pattern[1..])
    } else if pattern.contains('*') {
        // Simple wildcard
        let parts: Vec<&str> = pattern.split('*').collect();
        if parts.len() == 2 {
            text.starts_with(parts[0]) && text.ends_with(parts[1])
        } else {
            false
        }
    } else {
        text.contains(pattern)
    }
}

/// Truncate content to approximately max_tokens.
fn truncate_to_tokens(content: &str, max_tokens: usize, _token_counter: &TokenCounter) -> String {
    let chars_per_token = 4;
    let max_chars = max_tokens * chars_per_token;

    if content.len() <= max_chars {
        return content.to_string();
    }

    // Take head and tail, snapped to UTF-8 boundaries
    let mut head_end = (max_chars / 2).min(content.len());
    while !content.is_char_boundary(head_end) {
        head_end -= 1;
    }
    let mut tail_start = content.len().saturating_sub(max_chars - max_chars / 2);
    while !content.is_char_boundary(tail_start) {
        tail_start += 1;
    }
    let tail_start = tail_start.max(head_end);

    format!(
        "{}...[truncated {} bytes]...{}",
        &content[..head_end],
        tail_start - head_end,
        &content[tail_start..]
    )
}

/// Build context string from included files.
pub fn build_file_context(files: &[IncludedFile]) -> String {
    if files.is_empty() {
        return String::new();
    }

    let mut context = String::from("\n\n## Relevant Files\n");

    for file in files {
        context.push_str(&format!(
            "\n### {} ({} tokens, score: {:.1})\n",
            file.path.display(),
            file.tokens,
            file.score
        ));
        context.push_str("```\n");
        context.push_str(&file.content);
        context.push_str("\n```\n");
    }

    context
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::tokens::TokenCounter;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn test_extract_search_terms() {
        let task = "Refactor the auth module to add JWT validation in the login function";
        let terms = extract_search_terms(task);

        assert!(terms.iter().any(|t| t.contains("auth")));
        assert!(terms.iter().any(|t| t.contains("module")));
        assert!(terms.iter().any(|t| t.contains("JWT")));
        assert!(terms.iter().any(|t| t.contains("validation")));
        assert!(terms.iter().any(|t| t.contains("login")));
        assert!(terms.iter().any(|t| t.contains("function")));
    }

    #[test]
    fn test_extract_search_terms_snake_case() {
        let task = "Fix the user_auth_token validation in process_payment";
        let terms = extract_search_terms(task);

        assert!(terms.iter().any(|t| t.contains("user_auth_token")));
        assert!(terms.iter().any(|t| t.contains("process_payment")));
    }

    #[test]
    fn test_extract_search_terms_filters_common() {
        let task = "The function should work with the data from the user";
        let terms = extract_search_terms(task);

        // Common words should be filtered
        assert!(!terms.iter().any(|t| t == "the"));
        assert!(!terms.iter().any(|t| t == "and"));
        assert!(!terms.iter().any(|t| t == "with"));
        assert!(!terms.iter().any(|t| t == "from"));
    }

    #[test]
    fn test_glob_match() {
        assert!(glob_match("*.rs", "test.rs"));
        assert!(glob_match("*.rs", "src/test.rs"));
        assert!(!glob_match("*.rs", "test.ts"));

        assert!(glob_match("target/", "target/debug"));
        assert!(glob_match("target/", "src/target/debug"));

        assert!(glob_match("src/*", "src/main.rs"));
        assert!(glob_match("src/*", "src/lib.rs"));
    }

    #[test]
    fn test_truncate_to_tokens() {
        let counter = TokenCounter;
        let long_text = "a".repeat(10000);
        let truncated = truncate_to_tokens(&long_text, 100, &counter);

        assert!(truncated.contains("[truncated"));
        assert!(truncated.len() < long_text.len());
        assert!(counter.count(&truncated) <= 120); // Some overhead
    }

    #[test]
    fn test_find_relevant_files() {
        let dir = tempdir().unwrap();
        let root = dir.path();

        // Create test files
        fs::write(root.join("auth.rs"), "pub fn login() { validate_token(); }").unwrap();
        fs::write(
            root.join("payment.rs"),
            "pub fn process_payment() { charge_card(); }",
        )
        .unwrap();
        fs::write(root.join("utils.rs"), "pub fn helper() { }").unwrap();
        fs::create_dir_all(root.join("target")).unwrap();
        fs::write(root.join("target").join("debug.rs"), "compiled").unwrap();

        let config = SelectiveContextConfig {
            max_files: 5,
            max_tokens_per_file: 1000,
            include_patterns: vec!["*.rs".to_string()],
            exclude_patterns: vec!["target/".to_string()],
            follow_symlinks: false,
            auto_scan: true,
        };

        let counter = TokenCounter;
        let files =
            find_relevant_files(root, "login function validate token", &config, &counter).unwrap();

        assert!(!files.is_empty());
        // Should find auth.rs
        assert!(
            files
                .iter()
                .any(|f| f.path.to_string_lossy().contains("auth.rs"))
        );
        // Should not include target/
        assert!(
            !files
                .iter()
                .any(|f| f.path.to_string_lossy().contains("target"))
        );
    }

    #[test]
    fn test_truncate_is_utf8_safe() {
        let counter = TokenCounter;
        let text = "é".repeat(5000);
        let out = truncate_to_tokens(&text, 100, &counter);
        assert!(out.contains("[truncated"));
    }

    #[test]
    fn test_extract_mentions() {
        let m = extract_mentions("fix @src/main.rs, and (@lib.rs). mail a@b.com @user");
        assert_eq!(m, vec!["src/main.rs", "lib.rs"]);
    }

    #[test]
    fn test_expand_mentions_stays_in_root() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("proj");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src").join("a.rs"), "fn a() {}").unwrap();
        fs::write(dir.path().join("secret.txt"), "nope").unwrap();

        let config = SelectiveContextConfig::default();
        let files = expand_mentions(
            &root,
            "see @src/a.rs and @../secret.txt",
            &config,
            &TokenCounter,
        );
        assert_eq!(files.len(), 1);
        assert!(files[0].content.contains("fn a()"));
    }

    #[test]
    fn test_scan_excludes_dirs_with_windows_separators() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("target")).unwrap();
        fs::write(root.join("target").join("x.rs"), "fn login() {}").unwrap();
        let config = SelectiveContextConfig::default();
        let files = find_relevant_files(root, "login function", &config, &TokenCounter).unwrap();
        assert!(files.is_empty());
    }

    #[test]
    fn test_build_file_context() {
        let files = vec![IncludedFile {
            path: PathBuf::from("auth.rs"),
            content: "pub fn login() {}".to_string(),
            tokens: 10,
            score: 5.0,
            match_reasons: vec!["login".to_string()],
        }];

        let context = build_file_context(&files);
        assert!(context.contains("Relevant Files"));
        assert!(context.contains("auth.rs"));
        assert!(context.contains("login"));
    }

    #[test]
    fn test_selective_context_config_default() {
        let config = SelectiveContextConfig::default();
        assert_eq!(config.max_files, 10);
        assert_eq!(config.max_tokens_per_file, 2000);
        assert!(config.include_patterns.contains(&"*.rs".to_string()));
        assert!(config.exclude_patterns.contains(&"target/".to_string()));
    }
}
