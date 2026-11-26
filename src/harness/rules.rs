//! Permission rules: `deny` / `ask` / `allow` entries of the form `tool` or
//! `tool:pattern`, and the `run` command analysis they match against.
//!
//! ```text
//! "write"                 every call of the tool
//! "mcp__github__*"        wildcard on the tool name
//! "run:cargo test"        command prefix (whole words)
//! "run:git push*"         wildcard over the command line
//! "write:src/**"          wildcard over the target path
//! "read:**/.env*"
//! ```
//!
//! `*` matches any run of characters (including `/`) and `?` any single one.
//! `run` is spawned without a shell, so a command line is just the program
//! (lowercase, no directory or `.exe`) followed by its arguments.

use serde_json::Value;

pub const RUN_TOOL: &str = "run";

/// Programs where the first non-flag argument selects the actual operation.
const SUBCOMMAND_PROGRAMS: &[&str] = &[
    "git", "cargo", "npm", "pnpm", "yarn", "docker", "kubectl", "go", "dotnet", "pip", "uv", "gh",
    "rustup", "just",
];

/// Programs that can run arbitrary code, escalate, delete, or move data
/// off-machine; never covered by a session-wide grant.
const RISKY_PROGRAMS: &[&str] = &[
    "sh",
    "bash",
    "zsh",
    "fish",
    "dash",
    "cmd",
    "powershell",
    "pwsh",
    "env",
    "sudo",
    "su",
    "doas",
    "xargs",
    "eval",
    "nohup",
    "rm",
    "rmdir",
    "del",
    "rd",
    "erase",
    "curl",
    "wget",
    "ssh",
    "scp",
    "sftp",
    "rsync",
    "nc",
    "ncat",
    "dd",
    "mkfs",
    "chmod",
    "chown",
    "python",
    "python3",
    "node",
    "perl",
    "ruby",
    "deno",
    "bun",
    "npx",
    "bunx",
    "uvx",
    "pnpx",
    "find",
    "make",
];

const RISKY_SUBCOMMANDS: &[(&str, &str)] = &[
    ("git", "push"),
    ("git", "reset"),
    ("git", "clean"),
    ("git", "rebase"),
    ("git", "checkout"),
    ("git", "restore"),
    ("git", "rm"),
    ("git", "config"),
    ("npm", "publish"),
    ("npm", "exec"),
    ("pnpm", "dlx"),
    ("pnpm", "exec"),
    ("yarn", "dlx"),
    ("cargo", "publish"),
    ("cargo", "install"),
    ("docker", "run"),
    ("docker", "exec"),
    ("gh", "api"),
    ("pip", "install"),
    ("rustup", "run"),
    ("uv", "run"),
];

/// Global options (those that may precede the subcommand) that consume the
/// next argument, per program.
fn value_flags(program: &str) -> &'static [&'static str] {
    match program {
        "git" => &[
            "-c",
            "-C",
            "--git-dir",
            "--work-tree",
            "--namespace",
            "--exec-path",
            "--super-prefix",
            "--config-env",
        ],
        "cargo" => &["-C", "-Z", "--config", "--color"],
        "npm" | "pnpm" | "yarn" => &[
            "--prefix",
            "--workspace",
            "-w",
            "--registry",
            "--userconfig",
            "--cwd",
            "--dir",
            "-C",
            "--filter",
            "-F",
        ],
        "docker" => &[
            "-H",
            "--host",
            "-c",
            "--context",
            "--config",
            "-l",
            "--log-level",
        ],
        "kubectl" => &[
            "-n",
            "--namespace",
            "--context",
            "--kubeconfig",
            "-s",
            "--server",
            "--user",
            "--cluster",
            "--token",
        ],
        "gh" => &["-R", "--repo"],
        "uv" => &[
            "--directory",
            "--project",
            "--config-file",
            "--python",
            "-p",
        ],
        "just" => &["--justfile", "-f", "--working-directory", "-d"],
        "go" | "dotnet" | "pip" | "rustup" => &[],
        _ => &[],
    }
}

/// Global options known to be boolean (consume nothing).
fn is_bool_flag(flag: &str) -> bool {
    matches!(
        flag,
        "-v" | "-q"
            | "-s"
            | "-h"
            | "-p"
            | "-V"
            | "--verbose"
            | "--quiet"
            | "--silent"
            | "--help"
            | "--version"
            | "--no-pager"
            | "--paginate"
            | "--bare"
            | "--no-color"
            | "--no-replace-objects"
            | "--literal-pathspecs"
            | "--frozen"
            | "--locked"
            | "--offline"
    )
}

/// A parsed `run` call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunCall {
    /// Lowercase program name without directory or `.exe`.
    pub program: String,
    /// Resolved subcommand for subcommand-style programs.
    pub sub: Option<String>,
    /// Program and arguments exactly as given.
    pub raw_line: String,
    /// Program, subcommand and the arguments after it, with the global options
    /// that precede the subcommand removed. `None` when a leading option could
    /// not be understood, so the real subcommand is unknown.
    pub normalized_line: Option<String>,
    /// Every argument (lowercase), kept for defence in depth.
    args_lower: Vec<String>,
}

impl RunCall {
    /// Parse the arguments of a `run` call.
    pub fn parse(args: &Value) -> Option<Self> {
        let command = args.get("command")?.as_str()?;
        let base = command.rsplit(['/', '\\']).next()?.to_ascii_lowercase();
        let program = base.strip_suffix(".exe").unwrap_or(&base).to_string();
        if program.is_empty() {
            return None;
        }
        let argv: Vec<&str> = args
            .get("args")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
            .unwrap_or_default();

        let mut raw_line = program.clone();
        for a in &argv {
            raw_line.push(' ');
            raw_line.push_str(a);
        }
        let args_lower = argv.iter().map(|a| a.to_ascii_lowercase()).collect();

        let (sub, normalized_line) = if SUBCOMMAND_PROGRAMS.contains(&program.as_str()) {
            match find_subcommand(&program, &argv) {
                Some((idx, sub)) => {
                    let mut line = format!("{program} {sub}");
                    for a in &argv[idx + 1..] {
                        line.push(' ');
                        line.push_str(a);
                    }
                    (Some(sub), Some(line))
                }
                // Only options (e.g. `git --version`): nothing to select.
                None if argv.iter().all(|a| a.starts_with('-')) => (None, Some(raw_line.clone())),
                None => (None, None),
            }
        } else {
            (None, Some(raw_line.clone()))
        };

        Some(Self {
            program,
            sub,
            raw_line,
            normalized_line,
            args_lower,
        })
    }

    /// Session-grant / allow-rule key, e.g. `run:cargo test`. `None` when the
    /// subcommand could not be determined.
    pub fn key(&self) -> Option<String> {
        self.normalized_line.as_ref()?;
        Some(match &self.sub {
            Some(sub) => format!("{RUN_TOOL}:{} {sub}", self.program),
            None => format!("{RUN_TOOL}:{}", self.program),
        })
    }

    /// Whether this command may never be covered by a session-wide grant.
    pub fn is_risky(&self) -> bool {
        if RISKY_PROGRAMS.contains(&self.program.as_str()) {
            return true;
        }
        let hits = |sub: &str| RISKY_SUBCOMMANDS.contains(&(self.program.as_str(), sub));
        if self.sub.as_deref().is_some_and(hits) {
            return true;
        }
        // Defence in depth: a risky subcommand anywhere in the argument list.
        SUBCOMMAND_PROGRAMS.contains(&self.program.as_str())
            && self.args_lower.iter().any(|a| hits(a))
    }
}

/// Locate the subcommand: `(index in argv, lowercase name)`.
fn find_subcommand(program: &str, argv: &[&str]) -> Option<(usize, String)> {
    let takes_value = value_flags(program);
    let mut i = 0;
    while i < argv.len() {
        let a = argv[i];
        if a == "--" {
            return None;
        }
        if program == "cargo" && a.starts_with('+') {
            i += 1; // toolchain selector
            continue;
        }
        if let Some(rest) = a.strip_prefix('-') {
            if takes_value.contains(&a) {
                i += 2;
            } else if (rest.starts_with('-') && a.contains('='))
                || is_bool_flag(a)
                // Attached short value such as `-Cdir`.
                || takes_value.iter().any(|f| {
                    f.len() == 2 && !f.starts_with("--") && a.starts_with(f) && a.len() > 2
                })
            {
                i += 1;
            } else {
                return None; // unknown option: cannot tell whether it takes a value
            }
            continue;
        }
        return Some((i, a.to_ascii_lowercase()));
    }
    None
}

/// Rule key for a `run` call (see [`RunCall::key`]).
pub fn run_rule_key(args: &Value) -> Option<String> {
    RunCall::parse(args)?.key()
}

/// Whether a rule key (or call) is risky.
pub fn is_risky_run(args: &Value) -> bool {
    RunCall::parse(args).is_none_or(|c| c.is_risky())
}

/// `*` / `?` wildcard match over characters.
pub fn wildcard_match(pattern: &str, text: &str, case_insensitive: bool) -> bool {
    let norm = |s: &str| -> Vec<char> {
        if case_insensitive {
            s.chars().flat_map(|c| c.to_lowercase()).collect()
        } else {
            s.chars().collect()
        }
    };
    let (p, t) = (norm(pattern), norm(text));
    let (mut pi, mut ti) = (0, 0);
    let (mut star, mut mark) = (None, 0);
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
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// One permission rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    tool: String,
    pattern: Option<String>,
    raw: String,
}

/// What a rule is matched against for one call.
#[derive(Debug, Default)]
pub struct Subjects {
    /// Every plausible rendering of the call. Used by `deny` and `ask` rules,
    /// which should fire on any reading.
    pub all: Vec<String>,
    /// Only renderings that are certain. Used by `allow` rules.
    pub strict: Vec<String>,
    is_path: bool,
}

impl Subjects {
    /// Build the subjects of a call.
    pub fn of(tool: &str, args: &Value) -> Self {
        if tool == RUN_TOOL {
            let Some(call) = RunCall::parse(args) else {
                return Self::default();
            };
            let mut all = vec![call.raw_line.clone()];
            let mut strict = Vec::new();
            if let Some(n) = &call.normalized_line {
                if *n != call.raw_line {
                    all.push(n.clone());
                }
                strict.push(n.clone());
            }
            return Self {
                all,
                strict,
                is_path: false,
            };
        }
        let Some(path) = args.get("path").and_then(|v| v.as_str()) else {
            return Self::default();
        };
        let rel = normalize_path(path);
        let mut all = vec![rel];
        if let Ok(cwd) = std::env::current_dir() {
            let abs = normalize_path(&cwd.join(path).to_string_lossy());
            if !all.contains(&abs) {
                all.push(abs);
            }
        }
        Self {
            strict: all.clone(),
            all,
            is_path: true,
        }
    }
}

/// Whether every word of `pattern` (a trailing `*` ignored) occurs in `line`
/// in order, with the first word being the program itself.
fn words_in_order(pattern: &str, line: &str) -> bool {
    let pat = pattern.trim_end_matches('*').trim_end();
    let mut pw = pat.split_whitespace();
    let mut lw = line.split_whitespace();
    let (Some(p0), Some(l0)) = (pw.next(), lw.next()) else {
        return false;
    };
    if p0 != l0 || pat.contains(['*', '?']) {
        return false;
    }
    let mut rest = lw;
    pw.all(|p| rest.any(|l| l == p))
}

fn normalize_path(p: &str) -> String {
    let s = p.replace('\\', "/");
    s.strip_prefix("./").unwrap_or(&s).to_string()
}

impl Rule {
    /// Parse `tool` or `tool:pattern`.
    pub fn parse(s: &str) -> Result<Self, String> {
        let raw = s.trim();
        if raw.is_empty() {
            return Err("empty permission rule".into());
        }
        let (tool, pattern) = match raw.split_once(':') {
            Some((t, p)) => (t.trim(), Some(p.trim())),
            None => (raw, None),
        };
        if tool.is_empty() {
            return Err(format!("permission rule '{raw}' has no tool name"));
        }
        if pattern.is_some_and(str::is_empty) {
            return Err(format!("permission rule '{raw}' has an empty pattern"));
        }
        let pattern = pattern.map(|p| {
            if p.starts_with('~') {
                shellexpand::tilde(p).replace('\\', "/")
            } else {
                p.replace('\\', "/")
            }
        });
        Ok(Self {
            tool: tool.to_string(),
            pattern,
            raw: raw.to_string(),
        })
    }

    pub fn as_str(&self) -> &str {
        &self.raw
    }

    fn tool_matches(&self, tool: &str) -> bool {
        self.tool == tool
            || (self.tool.contains(['*', '?']) && wildcard_match(&self.tool, tool, false))
    }

    /// Whether the rule covers this call. `strict` selects the certain-only
    /// subjects (for `allow` rules).
    pub fn matches(&self, tool: &str, subjects: &Subjects, strict: bool) -> bool {
        if !self.tool_matches(tool) {
            return false;
        }
        let Some(pattern) = &self.pattern else {
            return true;
        };
        let list = if strict {
            &subjects.strict
        } else {
            &subjects.all
        };
        list.iter().any(|s| {
            // Deny/ask rules also match when the pattern words appear in
            // order after the program (`git --mystery push` vs `git push`).
            if !strict && !subjects.is_path && words_in_order(pattern, s) {
                return true;
            }
            if subjects.is_path {
                wildcard_match(pattern, s, cfg!(windows))
            } else if pattern.contains(['*', '?']) {
                wildcard_match(pattern, s, false)
            } else {
                s == pattern
                    || s.strip_prefix(pattern.as_str())
                        .is_some_and(|r| r.starts_with(' '))
            }
        })
    }
}

/// Ordered rule lists. Evaluation order is `deny` > `ask` > `allow`.
#[derive(Debug, Clone, Default)]
pub struct Rules {
    pub deny: Vec<Rule>,
    pub ask: Vec<Rule>,
    pub allow: Vec<Rule>,
}

impl Rules {
    /// Parse the three lists, reporting the first invalid entry.
    pub fn parse(deny: &[String], ask: &[String], allow: &[String]) -> Result<Self, String> {
        let p = |l: &[String]| {
            l.iter()
                .map(|s| Rule::parse(s))
                .collect::<Result<Vec<_>, _>>()
        };
        Ok(Self {
            deny: p(deny)?,
            ask: p(ask)?,
            allow: p(allow)?,
        })
    }

    pub fn denied_by(&self, tool: &str, subjects: &Subjects) -> Option<&Rule> {
        self.deny.iter().find(|r| r.matches(tool, subjects, false))
    }

    pub fn asked_by(&self, tool: &str, subjects: &Subjects) -> Option<&Rule> {
        self.ask.iter().find(|r| r.matches(tool, subjects, false))
    }

    pub fn allowed_by(&self, tool: &str, subjects: &Subjects) -> bool {
        self.allow.iter().any(|r| r.matches(tool, subjects, true))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn run(command: &str, args: &[&str]) -> Value {
        json!({"command": command, "args": args})
    }

    #[test]
    fn wildcard_basics() {
        assert!(wildcard_match("git push*", "git push --force", false));
        assert!(wildcard_match("**/.env*", "/a/b/.env.local", false));
        assert!(wildcard_match("src/**", "src/a/b.rs", false));
        assert!(!wildcard_match("src/**", "lib/a.rs", false));
        assert!(wildcard_match("a?c", "abc", false));
        assert!(wildcard_match("SRC/*", "src/x", true));
    }

    #[test]
    fn subcommand_skips_value_taking_global_flags() {
        let c = RunCall::parse(&run("git", &["-C", "repo", "push"])).unwrap();
        assert_eq!(c.sub.as_deref(), Some("push"));
        assert_eq!(c.key().as_deref(), Some("run:git push"));
        assert!(c.is_risky());

        let c = RunCall::parse(&run("cargo", &["+nightly", "publish"])).unwrap();
        assert_eq!(c.key().as_deref(), Some("run:cargo publish"));
        assert!(c.is_risky());

        let c = RunCall::parse(&run("npm", &["--prefix", "x", "publish"])).unwrap();
        assert_eq!(c.key().as_deref(), Some("run:npm publish"));
        assert!(c.is_risky());

        let c = RunCall::parse(&run("git", &["-c", "a=b", "status", "-s"])).unwrap();
        assert_eq!(c.normalized_line.as_deref(), Some("git status -s"));
        assert!(!c.is_risky());
    }

    #[test]
    fn unknown_leading_flag_is_unkeyable() {
        let c = RunCall::parse(&run("git", &["--mystery", "x", "push"])).unwrap();
        assert!(c.key().is_none());
        assert!(c.normalized_line.is_none());
        // Defence in depth still flags it.
        assert!(c.is_risky());
    }

    #[test]
    fn rule_prefix_and_wildcard() {
        let allow = Rule::parse("run:git status").unwrap();
        let s = Subjects::of("run", &run("git", &["status", "-s"]));
        assert!(allow.matches("run", &s, true));
        let s = Subjects::of("run", &run("git", &["-C", "x", "push"]));
        assert!(!allow.matches("run", &s, true));

        let deny = Rule::parse("run:git push*").unwrap();
        assert!(deny.matches("run", &s, false));
        let s = Subjects::of("run", &run("git", &["--mystery", "push"]));
        assert!(deny.matches("run", &s, false));
        assert!(!allow.matches("run", &s, true));
    }

    #[test]
    fn path_rules() {
        let r = Rule::parse("write:src/**").unwrap();
        assert!(r.matches(
            "write",
            &Subjects::of("write", &json!({"path": "./src/a.rs"})),
            false
        ));
        assert!(!r.matches(
            "write",
            &Subjects::of("write", &json!({"path": "docs/a.md"})),
            false
        ));
        let r = Rule::parse("read:**/.env*").unwrap();
        assert!(r.matches(
            "read",
            &Subjects::of("read", &json!({"path": "a\\.env.local"})),
            false
        ));
    }

    #[test]
    fn tool_name_wildcard_and_parse_errors() {
        let r = Rule::parse("mcp__github__*").unwrap();
        assert!(r.matches("mcp__github__issues", &Subjects::default(), false));
        assert!(!r.matches("mcp__other__x", &Subjects::default(), false));
        assert!(Rule::parse("").is_err());
        assert!(Rule::parse("run:").is_err());
        assert!(Rule::parse(":x").is_err());
    }
}
