//! Environment allowlist for spawned `run` / job processes (M33).
//!
//! With `permissions.env_allowlist` set, children get only [`BASELINE`] plus
//! the listed names (`*` / `?` globs allowed) instead of the inherited
//! environment minus secret-looking variables.

use std::sync::RwLock;

/// Variables every child needs to work at all; always passed in allowlist mode.
const BASELINE: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "LANG",
    "LC_*",
    "TERM",
    "TZ",
    "TMPDIR",
    "TEMP",
    "TMP",
    "SHELL",
    "PWD",
    "SYSTEMROOT",
    "SYSTEMDRIVE",
    "WINDIR",
    "COMSPEC",
    "PATHEXT",
    "USERPROFILE",
];

static ALLOWLIST: RwLock<Option<Vec<String>>> = RwLock::new(None);

/// Enable (`Some`) or disable (`None`) allowlist mode.
pub fn set_allowlist(list: Option<Vec<String>>) {
    *ALLOWLIST.write().unwrap_or_else(|e| e.into_inner()) = list;
}

/// Current allowlist, if the mode is enabled.
pub fn allowlist() -> Option<Vec<String>> {
    ALLOWLIST.read().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Whether `name` may be passed to a child under `list`. Names compare
/// case-insensitively (Windows semantics) so one config works everywhere.
pub fn is_allowed(name: &str, list: &[String]) -> bool {
    BASELINE
        .iter()
        .copied()
        .chain(list.iter().map(String::as_str))
        .any(|p| glob(&p.to_ascii_uppercase(), &name.to_ascii_uppercase()))
}

/// Validate one allowlist entry (a variable name or glob).
pub fn validate_entry(entry: &str) -> Result<(), String> {
    if entry.is_empty() || entry.contains(['=', ' ', '\0']) {
        return Err(format!(
            "invalid environment variable name or pattern '{entry}'"
        ));
    }
    Ok(())
}

fn glob(pattern: &str, text: &str) -> bool {
    let (p, t): (Vec<char>, Vec<char>) = (pattern.chars().collect(), text.chars().collect());
    let (mut pi, mut ti, mut star, mut mark) = (0, 0, None, 0);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|&c| c == '*')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn baseline_and_listed_names_pass() {
        let list = vec!["CARGO_*".to_string(), "MY_FLAG".to_string()];
        for n in ["PATH", "path", "LC_ALL", "CARGO_HOME", "my_flag"] {
            assert!(is_allowed(n, &list), "{n}");
        }
        for n in [
            "AWS_SECRET_ACCESS_KEY",
            "OPENAI_API_KEY",
            "MY_FLAGS",
            "HOMEPATH2",
        ] {
            assert!(!is_allowed(n, &list), "{n}");
        }
    }

    #[test]
    fn explicit_entry_overrides_secret_looking_name() {
        let list = vec!["GITHUB_TOKEN".to_string()];
        assert!(is_allowed("GITHUB_TOKEN", &list));
    }

    #[test]
    fn entry_validation() {
        assert!(validate_entry("FOO_*").is_ok());
        assert!(validate_entry("").is_err());
        assert!(validate_entry("A=B").is_err());
    }
}
