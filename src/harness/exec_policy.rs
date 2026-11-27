//! File-based exec-policy rules (M33).
//!
//! ```toml
//! [[rule]]
//! action = "deny"            # allow | ask | deny
//! pattern = "git push*"      # word prefix, or `*` / `?` glob
//! # tool = "run"             # optional, default "run"
//! ```
//!
//! Files: `$XDG_CONFIG_HOME/okeanos/exec-policy.toml` (user) and
//! `<project>/.okeanos/exec-policy.toml` (project). They add to the
//! `permissions.deny/ask/allow` lists, and `deny` > `ask` > `allow` still
//! applies. An untrusted project file can only add `deny`/`ask` rules.

use super::rules::{RUN_TOOL, Rule, Rules};
use std::path::{Path, PathBuf};
use std::sync::RwLock;

pub const FILE_NAME: &str = "exec-policy.toml";

static LOADED: RwLock<Option<Rules>> = RwLock::new(None);

/// Remember the rules loaded at startup so every policy built later shares them.
pub fn set_loaded(rules: Rules) {
    *LOADED.write().unwrap_or_else(|e| e.into_inner()) = Some(rules);
}

/// Rules loaded at startup (empty if none were set).
pub fn loaded() -> Rules {
    LOADED
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
        .unwrap_or_default()
}

/// The user-level file path (`$XDG_CONFIG_HOME/okeanos/exec-policy.toml`).
pub fn user_file() -> Option<PathBuf> {
    let home = std::env::var("XDG_CONFIG_HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("HOME").ok().map(|h| format!("{h}/.config")))?;
    Some(Path::new(&home).join("okeanos").join(FILE_NAME))
}

/// The project-level file path.
pub fn project_file(project_dir: &Path) -> PathBuf {
    project_dir.join(".okeanos").join(FILE_NAME)
}

#[cfg(feature = "config-file")]
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct FileDoc {
    #[serde(default)]
    rule: Vec<RuleDoc>,
}

#[cfg(feature = "config-file")]
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RuleDoc {
    action: String,
    pattern: String,
    tool: Option<String>,
}

/// Parse a policy file. With `allow_loosening = false`, `allow` rules are
/// dropped and counted in the returned number.
#[cfg(feature = "config-file")]
pub fn parse(text: &str, allow_loosening: bool) -> Result<(Rules, usize), String> {
    let doc: FileDoc = toml::from_str(text).map_err(|e| e.to_string())?;
    let mut rules = Rules::default();
    let mut dropped = 0;
    for (i, r) in doc.rule.iter().enumerate() {
        let tool = r.tool.as_deref().unwrap_or(RUN_TOOL);
        let rule = Rule::parse(&format!("{tool}:{}", r.pattern))
            .map_err(|e| format!("rule {}: {e}", i + 1))?;
        match r.action.trim().to_ascii_lowercase().as_str() {
            "deny" => rules.deny.push(rule),
            "ask" => rules.ask.push(rule),
            "allow" if allow_loosening => rules.allow.push(rule),
            "allow" => dropped += 1,
            other => {
                return Err(format!(
                    "rule {}: invalid action '{other}' (expected allow, ask or deny)",
                    i + 1
                ));
            }
        }
    }
    Ok((rules, dropped))
}

#[cfg(not(feature = "config-file"))]
pub fn parse(_text: &str, _allow_loosening: bool) -> Result<(Rules, usize), String> {
    Err("exec-policy files require the `config-file` feature".into())
}

/// Load one file; a missing file is not an error.
pub fn load_file(path: &Path, allow_loosening: bool) -> Result<(Rules, usize), String> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((Rules::default(), 0)),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    parse(&text, allow_loosening).map_err(|e| format!("{}: {e}", path.display()))
}

/// Load the user file, then the project file. Returns the rules and the
/// number of `allow` rules ignored because the project is not trusted.
pub fn load_all(project_dir: &Path, project_trusted: bool) -> Result<(Rules, usize), String> {
    let mut rules = Rules::default();
    let mut dropped = 0;
    if let Some(user) = user_file() {
        let (r, _) = load_file(&user, true)?;
        merge(&mut rules, r);
    }
    let (r, d) = load_file(&project_file(project_dir), project_trusted)?;
    merge(&mut rules, r);
    dropped += d;
    Ok((rules, dropped))
}

fn merge(into: &mut Rules, from: Rules) {
    into.deny.extend(from.deny);
    into.ask.extend(from.ask);
    into.allow.extend(from.allow);
}

#[cfg(all(test, feature = "config-file"))]
mod tests {
    use super::*;

    const DOC: &str = r#"
        [[rule]]
        action = "deny"
        pattern = "git push*"
        [[rule]]
        action = "ask"
        pattern = "cargo publish"
        [[rule]]
        action = "allow"
        pattern = "cargo test*"
        [[rule]]
        action = "deny"
        tool = "write"
        pattern = "secrets/**"
    "#;

    #[test]
    fn parses_actions_and_tool_default() {
        let (rules, dropped) = parse(DOC, true).unwrap();
        assert_eq!(
            (rules.deny.len(), rules.ask.len(), rules.allow.len()),
            (2, 1, 1)
        );
        assert_eq!(dropped, 0);
        assert_eq!(rules.deny[0].as_str(), "run:git push*");
        assert_eq!(rules.deny[1].as_str(), "write:secrets/**");
    }

    #[test]
    fn untrusted_drops_allow_rules_only() {
        let (rules, dropped) = parse(DOC, false).unwrap();
        assert!(rules.allow.is_empty());
        assert_eq!((rules.deny.len(), rules.ask.len(), dropped), (2, 1, 1));
    }

    #[test]
    fn rejects_bad_input() {
        assert!(parse("[[rule]]\naction = \"maybe\"\npattern = \"x\"", true).is_err());
        assert!(parse("[[rule]]\naction = \"deny\"\npattern = \"\"", true).is_err());
        assert!(parse("[[rule]]\naction = \"deny\"", true).is_err());
        assert!(parse("[[rules]]\naction = \"deny\"\npattern = \"x\"", true).is_err());
    }

    #[test]
    fn missing_file_is_empty_and_bad_file_errors() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join(FILE_NAME);
        let (rules, _) = load_file(&p, true).unwrap();
        assert!(rules.deny.is_empty());
        std::fs::write(&p, "not toml [").unwrap();
        assert!(load_file(&p, true).unwrap_err().contains(FILE_NAME));
    }
}
