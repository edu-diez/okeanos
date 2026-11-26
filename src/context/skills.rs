//! Skills with progressive disclosure.
//!
//! A skill is a directory `<root>/<name>/SKILL.md` (or a legacy flat `<root>/<name>.md`) with optional YAML-style
//! frontmatter (`name`, `description`). Only the name + description index is
//! placed in the system prompt; the body is loaded on demand by the `skill`
//! tool. This keeps the prompt prefix small and cache-stable.

use std::path::{Path, PathBuf};

/// Maximum description length shown in the index.
const MAX_DESCRIPTION_CHARS: usize = 300;

/// A discovered skill (metadata only; the body is read lazily).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    pub name: String,
    pub description: String,
    /// Path to the `SKILL.md` file.
    pub path: PathBuf,
}

impl Skill {
    /// Read the skill body (frontmatter stripped).
    pub fn read_body(&self) -> std::io::Result<String> {
        let raw = std::fs::read_to_string(&self.path)?;
        Ok(split_frontmatter(&raw).1.trim().to_string())
    }

    /// Directory containing the skill (for relative file references).
    pub fn dir(&self) -> &Path {
        self.path.parent().unwrap_or(Path::new("."))
    }
}

/// Candidate skill roots in priority order (earlier roots win on name clashes).
pub fn skill_roots(configured: Option<&Path>, project_root: &Path) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Some(dir) = configured {
        roots.push(dir.to_path_buf());
    }
    for rel in [".okeanos/skills", ".claude/skills", ".agents/skills"] {
        roots.push(project_root.join(rel));
    }
    if let Some(home) = dirs::home_dir() {
        for rel in [".okeanos/skills", ".claude/skills", ".agents/skills"] {
            roots.push(home.join(rel));
        }
    }
    roots
}

/// Discover skills under `roots`. Missing roots are ignored; the result is
/// sorted by name and deduplicated (first root wins).
pub fn discover_skills(roots: &[PathBuf]) -> Vec<Skill> {
    let mut found: Vec<Skill> = Vec::new();
    for root in roots {
        let Ok(entries) = std::fs::read_dir(root) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            // `<name>/SKILL.md`, or a legacy flat `<name>.md` file.
            let (skill_md, default_name) = if path.is_dir() {
                (
                    path.join("SKILL.md"),
                    entry.file_name().to_string_lossy().to_string(),
                )
            } else if path.extension().and_then(|e| e.to_str()) == Some("md") {
                let stem = path.file_stem().map(|s| s.to_string_lossy().to_string());
                (path.clone(), stem.unwrap_or_default())
            } else {
                continue;
            };
            let Ok(raw) = std::fs::read_to_string(&skill_md) else {
                continue;
            };
            let (front, body) = split_frontmatter(&raw);
            let name = front_value(front, "name").unwrap_or(default_name);
            if name.is_empty() || found.iter().any(|s| s.name == name) {
                continue;
            }
            let description = front_value(front, "description")
                .filter(|d| !d.is_empty())
                .unwrap_or_else(|| first_line(body));
            found.push(Skill {
                name,
                description: truncate_chars(&description, MAX_DESCRIPTION_CHARS),
                path: skill_md,
            });
        }
    }
    found.sort_by(|a, b| a.name.cmp(&b.name));
    found
}

/// Render the system-prompt index. Empty when there are no skills.
pub fn render_index(skills: &[Skill]) -> String {
    if skills.is_empty() {
        return String::new();
    }
    let mut out = String::from(
        "\n\n## Available skills\nCall the `skill` tool with a skill's name to load its full \
         instructions when the task matches its description.\n",
    );
    for s in skills {
        out.push_str(&format!("- {}: {}\n", s.name, s.description));
    }
    out
}

/// Split `---`-delimited frontmatter from the body.
fn split_frontmatter(raw: &str) -> (&str, &str) {
    let raw = raw.strip_prefix('\u{feff}').unwrap_or(raw);
    let Some(rest) = raw
        .strip_prefix("---\r\n")
        .or_else(|| raw.strip_prefix("---\n"))
    else {
        return ("", raw);
    };
    let mut offset = 0;
    for line in rest.split_inclusive('\n') {
        if line.trim_end() == "---" {
            return (&rest[..offset], &rest[offset + line.len()..]);
        }
        offset += line.len();
    }
    ("", raw)
}

/// Read a top-level `key: value` (supports quotes and `>`/`|` block scalars).
fn front_value(front: &str, key: &str) -> Option<String> {
    let mut lines = front.lines().peekable();
    while let Some(line) = lines.next() {
        let Some(rest) = line.strip_prefix(key).and_then(|r| r.strip_prefix(':')) else {
            continue;
        };
        let value = rest.trim();
        if matches!(value, ">" | "|" | ">-" | "|-" | "") {
            let mut parts = Vec::new();
            while let Some(next) = lines.peek() {
                if next.starts_with(' ') || next.starts_with('\t') {
                    parts.push(next.trim().to_string());
                    lines.next();
                } else {
                    break;
                }
            }
            return Some(parts.join(" "));
        }
        let unquoted = value
            .strip_prefix('"')
            .and_then(|v| v.strip_suffix('"'))
            .or_else(|| value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
            .unwrap_or(value);
        return Some(unquoted.to_string());
    }
    None
}

fn first_line(body: &str) -> String {
    body.lines()
        .map(|l| l.trim().trim_start_matches('#').trim())
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .to_string()
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max).collect();
        format!("{cut}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn write_skill(root: &Path, dir: &str, content: &str) {
        fs::create_dir_all(root.join(dir)).unwrap();
        fs::write(root.join(dir).join("SKILL.md"), content).unwrap();
    }

    #[test]
    fn discovers_with_frontmatter_and_fallbacks() {
        let dir = tempdir().unwrap();
        write_skill(
            dir.path(),
            "deploy",
            "---\nname: deploy\ndescription: \"Ship it\"\n---\n# Deploy\nSteps here\n",
        );
        write_skill(dir.path(), "plain", "# Plain skill\nBody text\n");
        write_skill(
            dir.path(),
            "folded",
            "---\ndescription: >\n  first part\n  second part\n---\nbody\n",
        );

        let skills = discover_skills(&[dir.path().to_path_buf()]);
        let names: Vec<_> = skills.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["deploy", "folded", "plain"]);
        assert_eq!(skills[0].description, "Ship it");
        assert_eq!(skills[1].description, "first part second part");
        assert_eq!(skills[2].description, "Plain skill");
        assert_eq!(skills[0].read_body().unwrap(), "# Deploy\nSteps here");
    }

    #[test]
    fn earlier_root_wins_and_index_is_sorted() {
        let a = tempdir().unwrap();
        let b = tempdir().unwrap();
        write_skill(a.path(), "x", "---\ndescription: from a\n---\nA");
        write_skill(b.path(), "x", "---\ndescription: from b\n---\nB");
        write_skill(b.path(), "a-skill", "---\ndescription: other\n---\nC");

        let skills = discover_skills(&[a.path().to_path_buf(), b.path().to_path_buf()]);
        let index = render_index(&skills);
        assert!(index.contains("- x: from a"));
        assert!(!index.contains("from b"));
        assert!(index.find("a-skill").unwrap() < index.find("- x:").unwrap());
    }

    #[test]
    fn legacy_flat_md_files_are_skills() {
        let tmp = tempdir().unwrap();
        fs::write(tmp.path().join("style.md"), "Use tabs.\nMore detail.").unwrap();
        fs::write(tmp.path().join("notes.txt"), "ignored").unwrap();
        let skills = discover_skills(&[tmp.path().to_path_buf()]);
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "style");
        assert_eq!(skills[0].description, "Use tabs.");
        assert!(skills[0].read_body().unwrap().contains("More detail."));
    }

    #[test]
    fn empty_roots_give_empty_index() {
        assert_eq!(
            render_index(&discover_skills(&[PathBuf::from("/nope")])),
            ""
        );
    }
}
