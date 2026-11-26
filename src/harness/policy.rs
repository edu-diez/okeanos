//! Permission policy: `deny` > `ask` > `allow` rules, permission modes,
//! confirm-before-write/delete/network, and session-scoped grants.
//!
//! Decision order for a call:
//!
//! 1. a matching `deny` rule denies it (always, in every mode);
//! 2. in `plan` mode anything that is not read-only is denied;
//! 3. a matching `ask` rule always asks (no grant or hook can skip it);
//! 4. `yolo` mode allows the rest;
//! 5. allow-list / `allow` rules and session grants allow it;
//! 6. `accept-edits` mode allows write-category tools;
//! 7. otherwise read tools run and write/delete/network tools ask.

use super::rules::{RUN_TOOL, Rule, Rules, RunCall, Subjects};
use crate::events::ApprovalDecision;
use crate::tool::ToolSpec;
use crate::tool::tool_trait::ToolCategory;
use serde_json::Value;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

pub use super::rules::run_rule_key;

/// How much the agent may do without asking.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum PermissionMode {
    /// Read freely; ask before write, delete and network/exec.
    #[default]
    Default,
    /// Like `default`, but file edits (write-category tools) are allowed.
    AcceptEdits,
    /// Read-only: everything that mutates is denied.
    Plan,
    /// Allow everything except what `deny`/`ask` rules say.
    Yolo,
}

impl PermissionMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::AcceptEdits => "accept-edits",
            Self::Plan => "plan",
            Self::Yolo => "yolo",
        }
    }

    /// Next mode in the interactive cycle.
    pub fn next(self) -> Self {
        match self {
            Self::Default => Self::AcceptEdits,
            Self::AcceptEdits => Self::Plan,
            Self::Plan => Self::Yolo,
            Self::Yolo => Self::Default,
        }
    }

    fn to_u8(self) -> u8 {
        match self {
            Self::Default => 0,
            Self::AcceptEdits => 1,
            Self::Plan => 2,
            Self::Yolo => 3,
        }
    }

    fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::AcceptEdits,
            2 => Self::Plan,
            3 => Self::Yolo,
            _ => Self::Default,
        }
    }
}

impl FromStr for PermissionMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "default" => Ok(Self::Default),
            "accept-edits" | "acceptedits" => Ok(Self::AcceptEdits),
            "plan" => Ok(Self::Plan),
            "yolo" | "bypass" => Ok(Self::Yolo),
            other => Err(format!(
                "invalid permission mode '{other}' (expected default, accept-edits, plan or yolo)"
            )),
        }
    }
}

impl std::fmt::Display for PermissionMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Shared, live-switchable permission mode. Clones observe the same value, so
/// the UI can change the mode of a running loop and its sub-agents.
#[derive(Debug, Clone, Default)]
pub struct ModeHandle(Arc<AtomicU8>);

impl ModeHandle {
    pub fn new(mode: PermissionMode) -> Self {
        Self(Arc::new(AtomicU8::new(mode.to_u8())))
    }

    pub fn get(&self) -> PermissionMode {
        PermissionMode::from_u8(self.0.load(Ordering::Relaxed))
    }

    pub fn set(&self, mode: PermissionMode) {
        self.0.store(mode.to_u8(), Ordering::Relaxed);
    }

    /// Advance to the next mode and return it.
    pub fn cycle(&self) -> PermissionMode {
        let next = self.get().next();
        self.set(next);
        next
    }
}

/// Policy decision for a tool call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicyDecision {
    Allow,
    /// Refused; the string is the reason shown to the model.
    Deny(String),
    /// Ask, unless something (hook, grant) approves it.
    RequireApproval,
    /// Ask because an explicit `ask` rule says so; nothing can skip it.
    AlwaysAsk,
}

/// Permission policy.
///
/// Safe by default: read-only operations run without confirmation; write,
/// delete, and network operations require explicit approval unless allowed by
/// a rule, a session grant, or the permission mode.
#[derive(Debug, Default, Clone)]
pub struct Policy {
    rules: Rules,
    session: Vec<Rule>,
    mode: ModeHandle,
}

impl Policy {
    /// Create a policy with the given allow-list. Entries are tool names
    /// (`write`) or rules (`run:cargo test`); invalid entries are ignored
    /// (config validation reports them).
    pub fn new(allow_list: Vec<String>) -> Self {
        Self {
            rules: Rules {
                allow: allow_list
                    .iter()
                    .filter_map(|s| Rule::parse(s).ok())
                    .collect(),
                ..Rules::default()
            },
            ..Self::default()
        }
    }

    /// Add `deny` / `ask` / `allow` rules.
    pub fn with_rules(mut self, rules: Rules) -> Self {
        self.rules.deny.extend(rules.deny);
        self.rules.ask.extend(rules.ask);
        self.rules.allow.extend(rules.allow);
        self
    }

    /// Set the starting permission mode.
    pub fn with_mode(self, mode: PermissionMode) -> Self {
        self.mode.set(mode);
        self
    }

    /// Handle for switching the mode while the loop runs.
    pub fn mode_handle(&self) -> ModeHandle {
        self.mode.clone()
    }

    pub fn mode(&self) -> PermissionMode {
        self.mode.get()
    }

    /// Policy for a sub-agent: the parent's `deny`/`ask` rules and (shared)
    /// mode, with the sub-agent's own tool allow-list.
    pub fn derive_for_subagent(&self, allow_list: Vec<String>) -> Self {
        let mut p = Self::new(allow_list);
        p.rules.deny = self.rules.deny.clone();
        p.rules.ask = self.rules.ask.clone();
        p.mode = self.mode.clone();
        p
    }

    /// Check a call by tool name only (no arguments).
    pub fn check(&self, tool: &str, spec: &ToolSpec) -> PolicyDecision {
        self.check_call(tool, spec, &Value::Null)
    }

    /// Argument-aware decision; see the module docs for the order.
    ///
    /// For `run`, allow rules and grants match the command (`run:cargo test`
    /// matches `cargo test --all`), so approving one command never approves
    /// arbitrary execution. A bare `run` allow entry still allows everything.
    pub fn check_call(&self, tool: &str, spec: &ToolSpec, args: &Value) -> PolicyDecision {
        let subjects = Subjects::of(tool, args);
        if let Some(rule) = self.rules.denied_by(tool, &subjects) {
            return PolicyDecision::Deny(format!(
                "'{tool}' denied by permission rule '{}'",
                rule.as_str()
            ));
        }

        let mode = self.mode.get();
        if mode == PermissionMode::Plan {
            let blocked = match spec.category {
                ToolCategory::Read => false,
                ToolCategory::Write | ToolCategory::Delete => true,
                ToolCategory::Network => !self.rules.allowed_by(tool, &subjects),
            };
            if blocked {
                return PolicyDecision::Deny(format!(
                    "'{tool}' is not available in plan mode (read-only); \
                     describe the change as a plan instead of making it"
                ));
            }
        }

        if self.rules.asked_by(tool, &subjects).is_some() {
            return PolicyDecision::AlwaysAsk;
        }
        if mode == PermissionMode::Yolo {
            return PolicyDecision::Allow;
        }
        if self.rules.allowed_by(tool, &subjects)
            || self
                .session
                .iter()
                .any(|r| r.matches(tool, &subjects, true))
        {
            return PolicyDecision::Allow;
        }
        if mode == PermissionMode::AcceptEdits && spec.category == ToolCategory::Write {
            return PolicyDecision::Allow;
        }

        match spec.category {
            ToolCategory::Read => PolicyDecision::Allow,
            ToolCategory::Write | ToolCategory::Delete | ToolCategory::Network => {
                PolicyDecision::RequireApproval
            }
        }
    }

    /// Grant allow-always for a whole tool in this session.
    pub fn allow_always(&mut self, tool: &str) {
        if let Ok(rule) = Rule::parse(tool) {
            self.session.push(rule);
        }
    }

    /// Grant allow-always for a specific call. `run` grants are scoped to the
    /// command (see [`run_rule_key`]) and refused for risky or unkeyable
    /// commands, which are asked about every time. Calls covered by an `ask`
    /// rule are never granted. Returns whether a grant was recorded.
    pub fn allow_always_call(&mut self, tool: &str, args: &Value) -> bool {
        if self
            .rules
            .asked_by(tool, &Subjects::of(tool, args))
            .is_some()
        {
            return false;
        }
        if tool != RUN_TOOL {
            self.allow_always(tool);
            return true;
        }
        let Some(call) = RunCall::parse(args) else {
            return false;
        };
        match call.key() {
            Some(key) if !call.is_risky() => match Rule::parse(&key) {
                Ok(rule) => {
                    self.session.push(rule);
                    true
                }
                Err(_) => false,
            },
            _ => false,
        }
    }

    /// Whether this specific call is covered by a session grant.
    pub fn has_session_allow_call(&self, tool: &str, args: &Value) -> bool {
        let subjects = Subjects::of(tool, args);
        self.session
            .iter()
            .any(|r| r.matches(tool, &subjects, true))
    }

    /// Record an approval decision, persisting allow-always grants.
    pub fn record_decision(&mut self, tool: &str, decision: ApprovalDecision) {
        if decision == ApprovalDecision::AllowAlways {
            self.allow_always(tool);
        }
    }

    /// Check if a tool has a whole-tool session grant.
    pub fn has_session_allow(&self, tool: &str) -> bool {
        self.session
            .iter()
            .any(|r| r.as_str() == tool && !tool.contains(':'))
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

    fn run_spec() -> ToolSpec {
        ToolSpec::new("run", "d", json!({})).with_category(ToolCategory::Network)
    }

    fn rules(deny: &[&str], ask: &[&str], allow: &[&str]) -> Rules {
        let s = |l: &[&str]| l.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        Rules::parse(&s(deny), &s(ask), &s(allow)).unwrap()
    }

    #[test]
    fn test_policy_allow_list() {
        let policy = Policy::new(vec!["todo".into(), "read".into()]);
        let todo_spec = ToolSpec::new("todo", "desc", json!({}));
        let write_spec = write_spec();

        assert_eq!(policy.check("todo", &todo_spec), PolicyDecision::Allow);
        assert_eq!(policy.check("read", &read_spec()), PolicyDecision::Allow);
        assert_eq!(
            policy.check("write", &write_spec),
            PolicyDecision::RequireApproval
        );
    }

    #[test]
    fn test_policy_read_tool_without_allow_list() {
        let policy = Policy::new(vec![]);
        assert_eq!(policy.check("read", &read_spec()), PolicyDecision::Allow);
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
        let spec = run_spec();
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
        let spec = run_spec();
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
    fn test_git_dash_c_grant_does_not_cover_push() {
        let mut policy = Policy::new(vec![]);
        let spec = run_spec();
        let status = json!({"command": "git", "args": ["-C", "repo", "status"]});
        let push = json!({"command": "git", "args": ["-C", "repo", "push"]});
        assert!(policy.allow_always_call("run", &status));
        assert_eq!(
            policy.check_call("run", &spec, &status),
            PolicyDecision::Allow
        );
        assert_eq!(
            policy.check_call("run", &spec, &push),
            PolicyDecision::RequireApproval
        );
        // A flag we cannot interpret is never granted nor matched by a grant.
        let odd = json!({"command": "git", "args": ["--mystery", "status"]});
        assert!(!policy.allow_always_call("run", &odd));
        assert_eq!(
            policy.check_call("run", &spec, &odd),
            PolicyDecision::RequireApproval
        );
    }

    #[test]
    fn test_run_allow_list_rules() {
        let spec = run_spec();
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

        assert!(!policy.has_session_allow("write"));
        assert_eq!(
            policy.check("write", &spec),
            PolicyDecision::RequireApproval
        );

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

    #[test]
    fn test_deny_beats_ask_and_allow() {
        let policy = Policy::new(vec!["run".into()]).with_rules(rules(
            &["run:git push*"],
            &["run:git commit*"],
            &[],
        ));
        let spec = run_spec();
        let push = json!({"command": "git", "args": ["push", "origin"]});
        let commit = json!({"command": "git", "args": ["commit", "-m", "x"]});
        let status = json!({"command": "git", "args": ["status"]});
        assert!(matches!(
            policy.check_call("run", &spec, &push),
            PolicyDecision::Deny(_)
        ));
        assert_eq!(
            policy.check_call("run", &spec, &commit),
            PolicyDecision::AlwaysAsk
        );
        assert_eq!(
            policy.check_call("run", &spec, &status),
            PolicyDecision::Allow
        );
    }

    #[test]
    fn test_deny_matches_through_unknown_flags() {
        let policy = Policy::new(vec![]).with_rules(rules(&["run:git push*"], &[], &[]));
        let call = json!({"command": "git", "args": ["-C", "x", "push", "--force"]});
        assert!(matches!(
            policy.check_call("run", &run_spec(), &call),
            PolicyDecision::Deny(_)
        ));
    }

    #[test]
    fn test_ask_rule_cannot_be_granted() {
        let mut policy = Policy::new(vec![]).with_rules(rules(&[], &["write:docs/**"], &[]));
        let call = json!({"path": "docs/a.md"});
        assert!(!policy.allow_always_call("write", &call));
        policy.allow_always("write");
        assert_eq!(
            policy.check_call("write", &write_spec(), &call),
            PolicyDecision::AlwaysAsk
        );
        assert_eq!(
            policy.check_call("write", &write_spec(), &json!({"path": "src/a.rs"})),
            PolicyDecision::Allow
        );
    }

    #[test]
    fn test_path_deny_rule_for_read() {
        let policy = Policy::new(vec![]).with_rules(rules(&["read:**/secrets/**"], &[], &[]));
        assert!(matches!(
            policy.check_call("read", &read_spec(), &json!({"path": "a/secrets/k.txt"})),
            PolicyDecision::Deny(_)
        ));
        assert_eq!(
            policy.check_call("read", &read_spec(), &json!({"path": "a/b.txt"})),
            PolicyDecision::Allow
        );
    }

    #[test]
    fn test_plan_mode_is_read_only() {
        let policy = Policy::new(vec!["write".into()]).with_mode(PermissionMode::Plan);
        assert!(matches!(
            policy.check("write", &write_spec()),
            PolicyDecision::Deny(_)
        ));
        let delete = ToolSpec::new("delete", "d", json!({})).with_category(ToolCategory::Delete);
        assert!(matches!(
            policy.check("delete", &delete),
            PolicyDecision::Deny(_)
        ));
        assert_eq!(policy.check("read", &read_spec()), PolicyDecision::Allow);
        // Network tools are denied unless explicitly allowed by a rule.
        let status = json!({"command": "git", "args": ["status"]});
        assert!(matches!(
            policy.check_call("run", &run_spec(), &status),
            PolicyDecision::Deny(_)
        ));
        let policy = Policy::new(vec!["run:git status".into()]).with_mode(PermissionMode::Plan);
        assert_eq!(
            policy.check_call("run", &run_spec(), &status),
            PolicyDecision::Allow
        );
    }

    #[test]
    fn test_accept_edits_and_yolo() {
        let accept = Policy::new(vec![]).with_mode(PermissionMode::AcceptEdits);
        assert_eq!(accept.check("write", &write_spec()), PolicyDecision::Allow);
        assert_eq!(
            accept.check_call("run", &run_spec(), &json!({"command": "ls"})),
            PolicyDecision::RequireApproval
        );

        let yolo = Policy::new(vec![])
            .with_mode(PermissionMode::Yolo)
            .with_rules(rules(&["run:rm*"], &["run:git push*"], &[]));
        assert_eq!(
            yolo.check_call("run", &run_spec(), &json!({"command": "ls"})),
            PolicyDecision::Allow
        );
        assert!(matches!(
            yolo.check_call("run", &run_spec(), &json!({"command": "rm", "args": ["x"]})),
            PolicyDecision::Deny(_)
        ));
        assert_eq!(
            yolo.check_call(
                "run",
                &run_spec(),
                &json!({"command": "git", "args": ["push"]})
            ),
            PolicyDecision::AlwaysAsk
        );
    }

    #[test]
    fn test_mode_handle_is_shared_with_subagent_policy() {
        let parent = Policy::new(vec![]).with_rules(rules(&["run:rm*"], &[], &[]));
        let child = parent.derive_for_subagent(vec!["write".into()]);
        assert_eq!(child.check("write", &write_spec()), PolicyDecision::Allow);
        assert!(matches!(
            child.check_call("run", &run_spec(), &json!({"command": "rm"})),
            PolicyDecision::Deny(_)
        ));
        parent.mode_handle().set(PermissionMode::Plan);
        assert!(matches!(
            child.check("write", &write_spec()),
            PolicyDecision::Deny(_)
        ));
    }

    #[test]
    fn test_mode_parse_and_cycle() {
        assert_eq!("accept-edits".parse(), Ok(PermissionMode::AcceptEdits));
        assert!("nope".parse::<PermissionMode>().is_err());
        let h = ModeHandle::default();
        assert_eq!(h.cycle(), PermissionMode::AcceptEdits);
        assert_eq!(h.cycle(), PermissionMode::Plan);
        assert_eq!(h.cycle(), PermissionMode::Yolo);
        assert_eq!(h.cycle(), PermissionMode::Default);
    }
}
