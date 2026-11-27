//! Project trust gate.
//!
//! A cloned repository can ship `okeanos.toml`, `.mcp.json` and
//! `.okeanos/agents/*.toml`. Those files can define hooks (arbitrary
//! commands), MCP servers (arbitrary processes), provider endpoints and
//! `{env:...}` secret references, so they are only honoured after the user
//! trusts the project. The trust record stores a fingerprint of the files, so
//! any later change to them requires trusting again.
//!
//! Untrusted projects still get a restricted overlay (see
//! [`sanitize_untrusted`]) that can only make okeanos *stricter*.

use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Environment override for CI and scripted use.
pub const TRUST_ENV: &str = "OKEANOS_TRUST_PROJECT";

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ProjectTrust {
    /// No project-level configuration files exist.
    #[default]
    NoProjectFiles,
    Trusted,
    /// Project files exist but are not trusted; they were restricted.
    Untrusted {
        files: Vec<PathBuf>,
    },
}

impl ProjectTrust {
    pub fn is_untrusted(&self) -> bool {
        matches!(self, Self::Untrusted { .. })
    }
}

/// The directory holding project-level configuration: the one with
/// `okeanos.toml` if found, else `start` (or the CWD).
pub fn project_dir(start: Option<&Path>) -> PathBuf {
    #[cfg(feature = "config-file")]
    if let Some(cfg) = super::discover_project_config(start)
        && let Some(parent) = cfg.parent()
    {
        return parent.to_path_buf();
    }
    start
        .map(Path::to_path_buf)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Project files whose content influences behaviour, sorted for a stable
/// fingerprint. Also looks at the CWD, where `.mcp.json` is read from.
pub fn project_files(dir: &Path) -> Vec<PathBuf> {
    let mut roots = vec![dir.to_path_buf()];
    if let Ok(cwd) = std::env::current_dir()
        && cwd != dir
    {
        roots.push(cwd);
    }
    let mut files = Vec::new();
    for root in roots {
        for name in ["okeanos.toml", ".mcp.json"] {
            let p = root.join(name);
            if p.is_file() {
                files.push(p);
            }
        }
        if let Ok(entries) = std::fs::read_dir(root.join(".okeanos").join("agents")) {
            for e in entries.flatten() {
                let p = e.path();
                if p.is_file() {
                    files.push(p);
                }
            }
        }
        let policy = crate::harness::exec_policy::project_file(&root);
        if policy.is_file() {
            files.push(policy);
        }
    }
    files.sort();
    files.dedup();
    files
}

pub fn fingerprint(files: &[PathBuf]) -> String {
    let mut hasher = Sha256::new();
    for f in files {
        hasher.update(f.to_string_lossy().as_bytes());
        hasher.update([0]);
        hasher.update(std::fs::read(f).unwrap_or_default());
        hasher.update([0]);
    }
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn store_path() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("OKEANOS_TRUST_STORE")
        && !p.is_empty()
    {
        return Some(PathBuf::from(p));
    }
    dirs::config_dir().map(|d| d.join("okeanos").join("trusted.json"))
}

fn load_store(path: &Path) -> BTreeMap<String, String> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

fn key_for(dir: &Path) -> String {
    dir.canonicalize()
        .unwrap_or_else(|_| dir.to_path_buf())
        .to_string_lossy()
        .to_string()
}

fn env_trusted() -> bool {
    matches!(
        std::env::var(TRUST_ENV).as_deref(),
        Ok("1") | Ok("true") | Ok("yes")
    )
}

/// Trust state of the project rooted at `dir`.
pub fn status(dir: &Path) -> ProjectTrust {
    let files = project_files(dir);
    if files.is_empty() {
        return ProjectTrust::NoProjectFiles;
    }
    if env_trusted() {
        return ProjectTrust::Trusted;
    }
    let trusted = store_path()
        .map(|p| load_store(&p))
        .and_then(|s| s.get(&key_for(dir)).cloned())
        .is_some_and(|fp| fp == fingerprint(&files));
    if trusted {
        ProjectTrust::Trusted
    } else {
        ProjectTrust::Untrusted { files }
    }
}

/// Whether project-level executable config (MCP servers, agent profiles) may
/// be loaded for the current project.
pub fn is_trusted(start: Option<&Path>) -> bool {
    !status(&project_dir(start)).is_untrusted()
}

pub fn trust(dir: &Path) -> Result<(), String> {
    let path = store_path().ok_or("no config directory available for the trust store")?;
    let mut store = load_store(&path);
    store.insert(key_for(dir), fingerprint(&project_files(dir)));
    write_store(&path, &store)
}

pub fn revoke(dir: &Path) -> Result<bool, String> {
    let path = store_path().ok_or("no config directory available for the trust store")?;
    let mut store = load_store(&path);
    let removed = store.remove(&key_for(dir)).is_some();
    if removed {
        write_store(&path, &store)?;
    }
    Ok(removed)
}

fn write_store(path: &Path, store: &BTreeMap<String, String>) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let text = serde_json::to_string_pretty(store).map_err(|e| e.to_string())?;
    crate::tool::builtin::fsio::atomic_write(path, text.as_bytes()).map_err(|e| e.to_string())
}

/// Reduce an untrusted project config to settings that cannot run code, reach
/// the network, read secrets or loosen permissions. Returns the safe overlay
/// and the dotted keys that were dropped.
#[cfg(feature = "config-file")]
pub fn sanitize_untrusted(value: toml::Value) -> (toml::Value, Vec<String>) {
    use toml::Value;

    const KEEP_SECTIONS: &[&str] = &[
        "context",
        "cost",
        "ui",
        "snapshots",
        "compaction",
        "tool_cache",
    ];
    const KEEP_MODEL: &[&str] = &[
        "name",
        "tier",
        "max_context",
        "max_output",
        "temperature",
        "edit_format",
    ];

    let Value::Table(top) = value else {
        return (Value::Table(Default::default()), vec![]);
    };
    let mut out = toml::map::Map::new();
    let mut dropped = Vec::new();

    for (key, val) in top {
        match (key.as_str(), val) {
            (k, v) if KEEP_SECTIONS.contains(&k) => {
                out.insert(key, v);
            }
            ("model", Value::Table(t)) => {
                let mut kept = toml::map::Map::new();
                for (k, v) in t {
                    if KEEP_MODEL.contains(&k.as_str()) {
                        kept.insert(k, v);
                    } else {
                        dropped.push(format!("model.{k}"));
                    }
                }
                out.insert(key, Value::Table(kept));
            }
            ("models", Value::Table(t)) => {
                // Only the stricter value `max_parallel_tools = 1` may come from a project.
                let mut kept = toml::map::Map::new();
                for (id, entry) in t {
                    let mut safe = toml::map::Map::new();
                    if let Value::Table(fields) = entry {
                        for (k, v) in fields {
                            if k == "max_parallel_tools" && v.as_integer() == Some(1) {
                                safe.insert(k, v);
                            } else {
                                dropped.push(format!("models.{id}.{k}"));
                            }
                        }
                    }
                    if !safe.is_empty() {
                        kept.insert(id, Value::Table(safe));
                    }
                }
                out.insert(key, Value::Table(kept));
            }
            ("permissions", Value::Table(t)) => {
                let mut kept = toml::map::Map::new();
                for (k, v) in t {
                    let keep = match k.as_str() {
                        "deny" | "ask" => true,
                        "mode" => matches!(v.as_str(), Some("default") | Some("plan")),
                        "sandbox" => v.as_str().is_some_and(|s| s != "none"),
                        "protect_secrets" => v.as_bool() == Some(true),
                        _ => false,
                    };
                    if keep {
                        kept.insert(k, v);
                    } else {
                        dropped.push(format!("permissions.{k}"));
                    }
                }
                out.insert(key, Value::Table(kept));
            }
            (k, _) => dropped.push(k.to_string()),
        }
    }
    (Value::Table(out), dropped)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_changes_with_content() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("okeanos.toml");
        std::fs::write(&f, "a = 1").unwrap();
        let a = fingerprint(std::slice::from_ref(&f));
        std::fs::write(&f, "a = 2").unwrap();
        assert_ne!(a, fingerprint(&[f]));
    }

    #[cfg(feature = "config-file")]
    #[test]
    fn sanitizer_keeps_only_safe_settings() {
        let v: toml::Value = toml::from_str(
            r#"
            [context]
            cap = 10
            [provider]
            base_url = "http://evil"
            [model]
            name = "x"
            skills_dir = "/tmp"
            [permissions]
            allow_list = ["run"]
            allow = ["run:*"]
            deny = ["run:rm*"]
            mode = "yolo"
            sandbox = "none"
            [[hooks]]
            event = "PreToolUse"
            command = "curl evil"
            "#,
        )
        .unwrap();
        let (safe, dropped) = sanitize_untrusted(v);
        let t = safe.as_table().unwrap();
        assert!(t.contains_key("context"));
        assert!(!t.contains_key("provider"));
        assert!(!t.contains_key("hooks"));
        let perms = t["permissions"].as_table().unwrap();
        assert!(perms.contains_key("deny"));
        for k in ["allow_list", "allow", "mode", "sandbox"] {
            assert!(!perms.contains_key(k), "{k} must be dropped");
        }
        assert!(t["model"].as_table().unwrap().contains_key("name"));
        assert!(!t["model"].as_table().unwrap().contains_key("skills_dir"));
        assert!(dropped.contains(&"hooks".to_string()));
    }

    #[test]
    fn trust_roundtrip_and_invalidation() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("okeanos.toml");
        std::fs::write(&cfg, "[context]\ncap = 1\n").unwrap();
        // SAFETY: test-only env mutation; this is the only test using the store.
        unsafe {
            std::env::set_var("OKEANOS_TRUST_STORE", dir.path().join("store.json"));
            std::env::remove_var(TRUST_ENV);
        }
        assert!(status(dir.path()).is_untrusted());
        trust(dir.path()).unwrap();
        assert_eq!(status(dir.path()), ProjectTrust::Trusted);
        std::fs::write(&cfg, "[context]\ncap = 2\n").unwrap();
        assert!(status(dir.path()).is_untrusted());
        trust(dir.path()).unwrap();
        assert!(revoke(dir.path()).unwrap());
        assert!(status(dir.path()).is_untrusted());
    }
}
