//! Approval policy wiring (stub for Milestone 2).
//!
//! The full permission layer is implemented in Milestone 5.
//! This stub allows the ReAct loop to compile and run without approval checks.

use crate::events::ApprovalDecision;
use crate::tool::ToolSpec;
use crate::tool::tool_trait::ToolCategory;
use serde_json::Value;
use std::collections::HashSet;

const RUN_TOOL: &str = "run";

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
];

const RISKY_SUBCOMMANDS: &[(&str, &str)] = &[
    ("git", "push"),
    ("git", "reset"),
    ("git", "clean"),
    ("git", "rebase"),
    ("git", "checkout"),
    ("git", "restore"),
    ("git", "rm"),
    ("npm", "publish"),
    ("npm", "exec"),
    ("cargo", "publish"),
    ("cargo", "install"),
    ("docker", "run"),
    ("docker", "exec"),
    ("gh", "api"),
    ("pip", "install"),
];

/// Rule key for a `run` call: the lowercase program name (no directory or
/// `.exe`), plus the first non-flag argument for subcommand-style programs.
pub fn run_rule_key(args: &Value) -> Option<String> {
    let command = args.get("command")?.as_str()?;
    let base = command.rsplit(['/', '\\']).next()?.to_ascii_lowercase();
    let program = base.strip_suffix(".exe").unwrap_or(&base).to_string();
    if program.is_empty() {
        return None;
    }
    let mut key = format!("{RUN_TOOL}:{program}");
    if SUBCOMMAND_PROGRAMS.contains(&program.as_str())
        && let Some(sub) = args.get("args").and_then(|v| v.as_array()).and_then(|a| {
            a.iter()
                .filter_map(|v| v.as_str())
                .find(|s| !s.starts_with('-'))
        })
    {
        key.push(' ');
        key.push_str(&sub.to_ascii_lowercase());
    }
    Some(key)
}

fn is_risky_run(key: &str) -> bool {
    let rest = key.strip_prefix("run:").unwrap_or(key);
    let (program, sub) = rest.split_once(' ').unwrap_or((rest, ""));
    RISKY_PROGRAMS.contains(&program) || RISKY_SUBCOMMANDS.contains(&(program, sub))
}

fn rule_matches(rule: &str, key: &str) -> bool {
    rule.starts_with("run:")
        && (rule == key || key.strip_prefix(rule).is_some_and(|r| r.starts_with(' ')))
}

/// Policy decision for a tool call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicyDecision {
    Allow,
    Deny,
    RequireApproval,
}

/// Permission policy: allow-list + confirm-before-write/delete/network +
/// session-scoped allow-always grants.
///
/// Safe by default: read-only operations run without confirmation; write,
/// delete, and network operations require explicit approval unless on the
/// allow-list or session-granted via allow-always.
#[derive(Debug, Default)]
pub struct Policy {
    allow_list: HashSet<String>,
    session_allows: HashSet<String>,
}

impl Policy {
    /// Create a new policy with the given allow-list.
    pub fn new(allow_list: Vec<String>) -> Self {
        Self {
            allow_list: allow_list.into_iter().collect(),
            session_allows: HashSet::new(),
        }
    }

    /// Check if a tool call is allowed, based on allow-list, session grants,
    /// and tool category.
    ///
    /// - Tools on the allow-list are always allowed.
    /// - Tools with a session allow-always grant are always allowed.
    /// - Read-category tools are allowed without confirmation.
    /// - Write/delete/network tools require approval unless allow-listed or
    ///   session-granted.
    pub fn check(&self, tool: &str, spec: &ToolSpec) -> PolicyDecision {
        if self.allow_list.contains(tool) || self.session_allows.contains(tool) {
            return PolicyDecision::Allow;
        }

        match spec.category {
            ToolCategory::Read => PolicyDecision::Allow,
            ToolCategory::Write | ToolCategory::Delete | ToolCategory::Network => {
                PolicyDecision::RequireApproval
            }
        }
    }

    /// Argument-aware variant of [`Policy::check`].
    ///
    /// For `run`, allow-list entries and session grants are command rules
    /// (`run:cargo test` matches `cargo test --all`), so approving one command
    /// never approves arbitrary execution. A bare `run` allow-list entry still
    /// allows every command.
    pub fn check_call(&self, tool: &str, spec: &ToolSpec, args: &Value) -> PolicyDecision {
        if tool == RUN_TOOL
            && !self.allow_list.contains(tool)
            && !self.session_allows.contains(tool)
            && let Some(key) = run_rule_key(args)
        {
            let matches = |rule: &String| rule_matches(rule, &key);
            if self.allow_list.iter().any(matches) || self.session_allows.iter().any(matches) {
                return PolicyDecision::Allow;
            }
            return PolicyDecision::RequireApproval;
        }
        self.check(tool, spec)
    }

    /// Grant allow-always for a tool in this session.
    pub fn allow_always(&mut self, tool: &str) {
        self.session_allows.insert(tool.to_string());
    }

    /// Grant allow-always for a specific call. `run` grants are scoped to the
    /// command (see [`run_rule_key`]) and refused for risky commands, which
    /// are asked about every time. Returns whether a grant was recorded.
    pub fn allow_always_call(&mut self, tool: &str, args: &Value) -> bool {
        if tool != RUN_TOOL {
            self.allow_always(tool);
            return true;
        }
        match run_rule_key(args) {
            Some(key) if !is_risky_run(&key) => {
                self.session_allows.insert(key);
                true
            }
            _ => false,
        }
    }

    /// Whether this specific call is covered by a session grant.
    pub fn has_session_allow_call(&self, tool: &str, args: &Value) -> bool {
        if tool == RUN_TOOL
            && let Some(key) = run_rule_key(args)
        {
            return self.session_allows.iter().any(|r| rule_matches(r, &key));
        }
        self.has_session_allow(tool)
    }

    /// Record an approval decision, persisting allow-always grants.
    pub fn record_decision(&mut self, tool: &str, decision: ApprovalDecision) {
        if decision == ApprovalDecision::AllowAlways {
            self.allow_always(tool);
        }
    }

    /// Check if a tool has a session allow-always grant.
    pub fn has_session_allow(&self, tool: &str) -> bool {
        self.session_allows.contains(tool)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::ToolSpec;
    use crate::tool::tool_trait::ToolCategory;
    use serde_json::json;

    fn write_spec() -> ToolSpec {
        ToolSpec::new("write", "desc", json!({})).with_category(ToolCategory::Write)
    }

    fn read_spec() -> ToolSpec {
        ToolSpec::new("read", "desc", json!({}))
    }

    #[test]
    fn test_policy_allow_list() {
        let policy = Policy::new(vec!["todo".into(), "read".into()]);
        let todo_spec = ToolSpec::new("todo", "desc", json!({}));
        let write_spec = write_spec();

        // Allow-listed tools are always allowed regardless of category.
        assert_eq!(policy.check("todo", &todo_spec), PolicyDecision::Allow);
        assert_eq!(policy.check("read", &read_spec()), PolicyDecision::Allow);
        // Write tool is NOT on the allow-list -> requires approval.
        assert_eq!(
            policy.check("write", &write_spec),
            PolicyDecision::RequireApproval
        );
    }

    #[test]
    fn test_policy_read_tool_without_allow_list() {
        let policy = Policy::new(vec![]);
        // Read-category tools are allowed by default (safe by default).
        assert_eq!(policy.check("read", &read_spec()), PolicyDecision::Allow);
        // Write-category tools require approval unless allow-listed.
        assert_eq!(
            policy.check("write", &write_spec()),
            PolicyDecision::RequireApproval
        );
    }

    #[test]
    fn test_policy_session_allow_always() {
        let mut policy = Policy::new(vec!["todo".into()]);
        let spec = write_spec();

        assert_eq!(
            policy.check("write", &spec),
            PolicyDecision::RequireApproval
        );
        policy.allow_always("write");
        assert_eq!(policy.check("write", &spec), PolicyDecision::Allow);
    }

    #[test]
    fn test_policy_record_decision() {
        let mut policy = Policy::new(vec![]);
        let spec = write_spec();

        policy.record_decision("write", ApprovalDecision::Allow);
        assert_eq!(
            policy.check("write", &spec),
            PolicyDecision::RequireApproval
        );

        policy.record_decision("write", ApprovalDecision::AllowAlways);
        assert_eq!(policy.check("write", &spec), PolicyDecision::Allow);
    }

    #[test]
    fn test_policy_delete_and_network_require_approval() {
        let policy = Policy::new(vec![]);
        let delete_spec =
            ToolSpec::new("delete", "desc", json!({})).with_category(ToolCategory::Delete);
        let network_spec =
            ToolSpec::new("fetch", "desc", json!({})).with_category(ToolCategory::Network);

        assert_eq!(
            policy.check("delete", &delete_spec),
            PolicyDecision::RequireApproval
        );
        assert_eq!(
            policy.check("fetch", &network_spec),
            PolicyDecision::RequireApproval
        );
    }

    #[test]
    fn test_run_allow_always_is_command_scoped() {
        let mut policy = Policy::new(vec![]);
        let spec = ToolSpec::new("run", "d", json!({})).with_category(ToolCategory::Network);
        let test = json!({"command": "cargo", "args": ["test", "--all"]});
        let publish = json!({"command": "cargo", "args": ["publish"]});
        let ls = json!({"command": "C:\\bin\\LS.exe", "args": ["-la"]});

        assert!(policy.allow_always_call("run", &test));
        assert_eq!(
            policy.check_call("run", &spec, &test),
            PolicyDecision::Allow
        );
        assert_eq!(
            policy.check_call("run", &spec, &publish),
            PolicyDecision::RequireApproval
        );
        assert_eq!(
            policy.check_call("run", &spec, &ls),
            PolicyDecision::RequireApproval
        );
        assert!(policy.has_session_allow_call("run", &test));
        assert!(!policy.has_session_allow_call("run", &publish));
    }

    #[test]
    fn test_run_risky_commands_never_granted() {
        let mut policy = Policy::new(vec![]);
        let spec = ToolSpec::new("run", "d", json!({})).with_category(ToolCategory::Network);
        for call in [
            json!({"command": "sh", "args": ["-c", "x"]}),
            json!({"command": "rm", "args": ["-rf", "x"]}),
            json!({"command": "git", "args": ["push"]}),
        ] {
            assert!(!policy.allow_always_call("run", &call));
            assert_eq!(
                policy.check_call("run", &spec, &call),
                PolicyDecision::RequireApproval
            );
        }
    }

    #[test]
    fn test_run_allow_list_rules() {
        let spec = ToolSpec::new("run", "d", json!({})).with_category(ToolCategory::Network);
        let policy = Policy::new(vec!["run:cargo".into(), "run:git status".into()]);
        let check = |c: Value| policy.check_call("run", &spec, &c);
        assert_eq!(
            check(json!({"command": "cargo", "args": ["build"]})),
            PolicyDecision::Allow
        );
        assert_eq!(
            check(json!({"command": "git", "args": ["status", "-s"]})),
            PolicyDecision::Allow
        );
        assert_eq!(
            check(json!({"command": "git", "args": ["push"]})),
            PolicyDecision::RequireApproval
        );
        assert_eq!(
            check(json!({"command": "cargofoo"})),
            PolicyDecision::RequireApproval
        );
        let all = Policy::new(vec!["run".into()]);
        assert_eq!(
            all.check_call("run", &spec, &json!({"command": "anything"})),
            PolicyDecision::Allow
        );
    }

    #[test]
    fn test_policy_has_session_allow() {
        let mut policy = Policy::new(vec![]);
        let spec = write_spec();

        // Initially no session allow
        assert!(!policy.has_session_allow("write"));
        assert_eq!(
            policy.check("write", &spec),
            PolicyDecision::RequireApproval
        );

        // Grant allow-always
        policy.allow_always("write");
        assert!(policy.has_session_allow("write"));
        assert_eq!(policy.check("write", &spec), PolicyDecision::Allow);
    }

    #[test]
    fn test_policy_record_decision_allow_always_updates_session_allow() {
        let mut policy = Policy::new(vec![]);
        let spec = write_spec();

        assert!(!policy.has_session_allow("write"));
        policy.record_decision("write", ApprovalDecision::AllowAlways);
        assert!(policy.has_session_allow("write"));
        assert_eq!(policy.check("write", &spec), PolicyDecision::Allow);
    }
}
