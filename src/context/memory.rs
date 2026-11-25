//! Durable, cross-session context (M28).
//!
//! Loads `AGENTS.md` / `CLAUDE.md` instruction files and the `MEMORY.md`
//! index once at startup and renders them as a system-prompt suffix. Loading
//! once keeps the request prefix byte-stable for provider prompt caches, and
//! keeping this text in the system prompt (outside the compactable history)
//! means compaction never summarises it away.

use crate::model::ModelTier;
use std::path::{Path, PathBuf};

/// Instruction file names, in order of preference within one directory.
const INSTRUCTION_FILES: [&str; 2] = ["AGENTS.md", "CLAUDE.md"];

/// Name of the memory index file inside a memory directory.
pub const MEMORY_INDEX_FILE: &str = "MEMORY.md";

const DEFAULT_MAX_INSTRUCTION_BYTES: usize = 16 * 1024;
const SMALL_TIER_MAX_INSTRUCTION_BYTES: usize = 4 * 1024;
const DEFAULT_MAX_INDEX_LINES: usize = 200;
const MAX_INDEX_BYTES: usize = 8 * 1024;

/// Settings for instruction/memory loading (from the `[memory]` config section).
#[derive(Debug, Clone)]
pub struct MemoryConfig {
    /// Load the memory index and expose the `remember` tool.
    pub enabled: bool,
    /// Load `AGENTS.md` / `CLAUDE.md` files.
    pub instructions: bool,
    /// Total byte cap across all instruction files.
    pub max_instruction_bytes: usize,
    /// Line cap per `MEMORY.md` index.
    pub max_index_lines: usize,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            instructions: true,
            max_instruction_bytes: DEFAULT_MAX_INSTRUCTION_BYTES,
            max_index_lines: DEFAULT_MAX_INDEX_LINES,
        }
    }
}

impl MemoryConfig {
    /// Build from the optional config section, falling back to defaults.
    pub fn from_section(section: Option<&crate::config::MemorySection>) -> Self {
        let mut cfg = Self::default();
        if let Some(s) = section {
            if let Some(v) = s.enabled {
                cfg.enabled = v;
            }
            if let Some(v) = s.instructions {
                cfg.instructions = v;
            }
            if let Some(v) = s.max_instruction_bytes {
                cfg.max_instruction_bytes = v;
            }
            if let Some(v) = s.max_index_lines {
                cfg.max_index_lines = v;
            }
        }
        cfg
    }
}

/// A loaded instruction file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstructionFile {
    pub path: PathBuf,
    pub content: String,
    /// Bytes dropped to respect the size cap (0 = complete).
    pub omitted_bytes: usize,
}

/// A loaded `MEMORY.md` index for one scope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryIndex {
    pub label: &'static str,
    pub dir: PathBuf,
    pub content: String,
    pub truncated: bool,
}

/// Everything durable that is injected into the system prompt.
#[derive(Debug, Clone, Default)]
pub struct ProjectContext {
    /// Lowest priority first (user dir, then project root … CWD).
    pub instructions: Vec<InstructionFile>,
    /// `None` when memory is disabled or the tier does not support it.
    pub memory: Option<Vec<MemoryIndex>>,
}

/// User-level okeanos directory (`$XDG_CONFIG_HOME/okeanos` or `~/.config/okeanos`).
pub fn user_config_dir() -> Option<PathBuf> {
    let base = std::env::var("XDG_CONFIG_HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| {
            std::env::var("HOME")
                .ok()
                .filter(|s| !s.is_empty())
                .map(|h| format!("{h}/.config"))
        })?;
    Some(Path::new(&base).join("okeanos"))
}

/// Project-scoped memory directory.
pub fn project_memory_dir(project_root: &Path) -> PathBuf {
    project_root.join(".okeanos").join("memory")
}

/// User-scoped memory directory.
pub fn user_memory_dir() -> Option<PathBuf> {
    user_config_dir().map(|d| d.join("memory"))
}

impl ProjectContext {
    /// Load instructions and memory indexes for a session.
    ///
    /// `user_dir` is the user-level okeanos directory (see [`user_config_dir`]).
    pub fn load(
        project_root: &Path,
        cwd: &Path,
        user_dir: Option<&Path>,
        tier: ModelTier,
        cfg: &MemoryConfig,
    ) -> Self {
        let mut ctx = Self::default();

        if cfg.instructions {
            let mut dirs: Vec<PathBuf> = Vec::new();
            if let Some(u) = user_dir {
                dirs.push(u.to_path_buf());
            }
            dirs.extend(walk_dirs(project_root, cwd));
            let budget = match tier {
                ModelTier::Small => cfg
                    .max_instruction_bytes
                    .min(SMALL_TIER_MAX_INSTRUCTION_BYTES),
                _ => cfg.max_instruction_bytes,
            };
            ctx.instructions = load_instructions(&dirs, budget);
        }

        if cfg.enabled && tier != ModelTier::Small {
            let mut indexes = Vec::new();
            let project_dir = project_memory_dir(project_root);
            indexes.push(load_index("Project", project_dir, cfg.max_index_lines));
            if let Some(u) = user_dir {
                indexes.push(load_index("User", u.join("memory"), cfg.max_index_lines));
            }
            ctx.memory = Some(indexes);
        }

        ctx
    }

    /// Render the system-prompt suffix. Empty when there is nothing to add.
    pub fn render(&self) -> String {
        let mut out = String::new();

        if !self.instructions.is_empty() {
            out.push_str("\n\n## Project instructions\n");
            out.push_str(
                "These come from instruction files in the user's environment. Later files \
                 are closer to the working directory and take precedence.\n",
            );
            for f in &self.instructions {
                out.push_str(&format!(
                    "\n### {}\n\n{}",
                    f.path.display(),
                    f.content.trim_end()
                ));
                if f.omitted_bytes > 0 {
                    out.push_str(&format!(
                        "\n[truncated: {} bytes omitted; `read` {} for the rest]",
                        f.omitted_bytes,
                        f.path.display()
                    ));
                }
                out.push('\n');
            }
        }

        if let Some(indexes) = &self.memory {
            out.push_str("\n\n## Memory\n");
            out.push_str(
                "Durable notes saved in earlier sessions. Each index line points to a topic \
                 file in the directory shown; `read` it when relevant. Use the `remember` \
                 tool to save facts worth keeping across sessions (preferences, conventions, \
                 decisions); do not save task-specific or transient details.\n",
            );
            for idx in indexes {
                out.push_str(&format!(
                    "\n### {} memory ({})\n\n",
                    idx.label,
                    idx.dir.display()
                ));
                if idx.content.trim().is_empty() {
                    out.push_str("(empty)\n");
                } else {
                    out.push_str(idx.content.trim_end());
                    out.push('\n');
                    if idx.truncated {
                        out.push_str(&format!(
                            "[index truncated; `read` {} for the rest]\n",
                            idx.dir.join(MEMORY_INDEX_FILE).display()
                        ));
                    }
                }
            }
        }

        out
    }
}

/// Directories from `project_root` down to `cwd` (root first).
fn walk_dirs(project_root: &Path, cwd: &Path) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if cwd.starts_with(project_root) {
        let mut cur = cwd.to_path_buf();
        loop {
            dirs.push(cur.clone());
            if cur == project_root || !cur.pop() {
                break;
            }
        }
        dirs.reverse();
    } else {
        dirs.push(project_root.to_path_buf());
    }
    dirs
}

/// Read the first instruction file found in each directory, then apply the
/// byte budget giving priority to the last (closest) files.
fn load_instructions(dirs: &[PathBuf], budget: usize) -> Vec<InstructionFile> {
    let mut found: Vec<(PathBuf, String)> = Vec::new();
    for dir in dirs {
        for name in INSTRUCTION_FILES {
            let path = dir.join(name);
            if !path.is_file() {
                continue;
            }
            match std::fs::read_to_string(&path) {
                Ok(text) if !text.trim().is_empty() => found.push((path, text)),
                Ok(_) => {}
                Err(e) => tracing::warn!("skipping instruction file {}: {e}", path.display()),
            }
            break;
        }
    }

    let mut remaining = budget;
    let mut out = Vec::new();
    for (path, text) in found.into_iter().rev() {
        let keep = truncate_at(&text, remaining);
        let omitted = text.len() - keep.len();
        remaining -= keep.len();
        // A file that doesn't fit at all is still listed so the model can `read` it.
        out.push(InstructionFile {
            path,
            content: keep.to_string(),
            omitted_bytes: omitted,
        });
    }
    out.reverse();
    out
}

fn load_index(label: &'static str, dir: PathBuf, max_lines: usize) -> MemoryIndex {
    let raw = std::fs::read_to_string(dir.join(MEMORY_INDEX_FILE)).unwrap_or_default();
    let mut truncated = false;
    let mut content = String::new();
    for (i, line) in raw.lines().enumerate() {
        if i >= max_lines || content.len() + line.len() + 1 > MAX_INDEX_BYTES {
            truncated = true;
            break;
        }
        content.push_str(line);
        content.push('\n');
    }
    MemoryIndex {
        label,
        dir,
        content,
        truncated,
    }
}

/// Longest prefix of `s` that is at most `max` bytes and ends on a char boundary.
fn truncate_at(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    #[test]
    fn instructions_ordered_root_to_cwd_with_user_first() {
        let tmp = tempdir().unwrap();
        let root = tmp.path().join("repo");
        let sub = root.join("a").join("b");
        let user = tmp.path().join("user");
        write(&user.join("AGENTS.md"), "user rules");
        write(&root.join("AGENTS.md"), "root rules");
        write(&sub.join("AGENTS.md"), "sub rules");

        let cfg = MemoryConfig::default();
        let ctx = ProjectContext::load(&root, &sub, Some(&user), ModelTier::Frontier, &cfg);
        let texts: Vec<_> = ctx
            .instructions
            .iter()
            .map(|f| f.content.as_str())
            .collect();
        assert_eq!(texts, ["user rules", "root rules", "sub rules"]);
    }

    #[test]
    fn agents_md_preferred_over_claude_md_with_fallback() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();
        write(&root.join("AGENTS.md"), "agents");
        write(&root.join("CLAUDE.md"), "claude");
        let ctx = ProjectContext::load(root, root, None, ModelTier::Mid, &MemoryConfig::default());
        assert_eq!(ctx.instructions.len(), 1);
        assert_eq!(ctx.instructions[0].content, "agents");

        std::fs::remove_file(root.join("AGENTS.md")).unwrap();
        let ctx = ProjectContext::load(root, root, None, ModelTier::Mid, &MemoryConfig::default());
        assert_eq!(ctx.instructions[0].content, "claude");
    }

    #[test]
    fn budget_favours_closest_file_and_reports_truncation() {
        let tmp = tempdir().unwrap();
        let root = tmp.path().join("repo");
        let sub = root.join("sub");
        write(&root.join("AGENTS.md"), &"r".repeat(100));
        write(&sub.join("AGENTS.md"), &"s".repeat(60));

        let cfg = MemoryConfig {
            max_instruction_bytes: 80,
            ..MemoryConfig::default()
        };
        let ctx = ProjectContext::load(&root, &sub, None, ModelTier::Frontier, &cfg);
        assert_eq!(ctx.instructions[1].content.len(), 60);
        assert_eq!(ctx.instructions[1].omitted_bytes, 0);
        assert_eq!(ctx.instructions[0].content.len(), 20);
        assert_eq!(ctx.instructions[0].omitted_bytes, 80);
        assert!(ctx.render().contains("[truncated: 80 bytes omitted"));
    }

    #[test]
    fn truncation_respects_utf8_boundaries() {
        assert_eq!(truncate_at("héllo", 2), "h");
        assert_eq!(truncate_at("héllo", 3), "hé");
    }

    #[test]
    fn small_tier_has_smaller_cap_and_no_memory() {
        let tmp = tempdir().unwrap();
        write(&tmp.path().join("AGENTS.md"), &"x".repeat(10_000));
        let ctx = ProjectContext::load(
            tmp.path(),
            tmp.path(),
            None,
            ModelTier::Small,
            &MemoryConfig::default(),
        );
        assert_eq!(
            ctx.instructions[0].content.len(),
            SMALL_TIER_MAX_INSTRUCTION_BYTES
        );
        assert!(ctx.memory.is_none());
    }

    #[test]
    fn memory_index_loaded_and_capped_by_lines() {
        let tmp = tempdir().unwrap();
        let root = tmp.path().join("repo");
        let lines: String = (0..10)
            .map(|i| format!("- [t{i}](t{i}.md) — d\n"))
            .collect();
        write(&project_memory_dir(&root).join(MEMORY_INDEX_FILE), &lines);

        let cfg = MemoryConfig {
            max_index_lines: 3,
            ..MemoryConfig::default()
        };
        let ctx = ProjectContext::load(&root, &root, None, ModelTier::Frontier, &cfg);
        let idx = &ctx.memory.as_ref().unwrap()[0];
        assert_eq!(idx.content.lines().count(), 3);
        assert!(idx.truncated);
        assert!(ctx.render().contains("[index truncated"));
    }

    #[test]
    fn disabled_config_renders_nothing() {
        let tmp = tempdir().unwrap();
        write(&tmp.path().join("AGENTS.md"), "rules");
        let cfg = MemoryConfig {
            enabled: false,
            instructions: false,
            ..MemoryConfig::default()
        };
        let ctx = ProjectContext::load(tmp.path(), tmp.path(), None, ModelTier::Frontier, &cfg);
        assert_eq!(ctx.render(), "");
    }

    #[test]
    fn cwd_outside_root_only_uses_root() {
        let tmp = tempdir().unwrap();
        let root = tmp.path().join("repo");
        let other = tmp.path().join("elsewhere");
        write(&root.join("AGENTS.md"), "root");
        write(&other.join("AGENTS.md"), "other");
        let ctx = ProjectContext::load(
            &root,
            &other,
            None,
            ModelTier::Mid,
            &MemoryConfig::default(),
        );
        assert_eq!(ctx.instructions.len(), 1);
        assert_eq!(ctx.instructions[0].content, "root");
    }
}
