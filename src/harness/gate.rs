//! The single gate every tool call passes through before it runs: `PreToolUse`
//! hooks, then the permission policy. The serial path, the parallel read batch
//! and sub-agents all use it, so hooks and rules cannot be bypassed by taking a
//! different execution path.

use super::hooks::{HookDecision, Hooks};
use super::policy::{Policy, PolicyDecision};
use crate::tool::ToolSpec;
use serde_json::Value;

/// What the gate decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateVerdict {
    /// Run the call.
    Allow,
    /// Ask the user first.
    Ask,
    /// Refuse; the string is the reason shown to the model.
    Deny(String),
}

/// The verdict plus the (possibly hook-rewritten) arguments to run with.
#[derive(Debug, Clone)]
pub struct GateOutcome {
    pub verdict: GateVerdict,
    pub args: Value,
}

/// Run hooks and policy for one call.
///
/// A hook `deny` blocks, and policy `deny` rules always win. A hook `ask`
/// forces a prompt; a hook `allow` skips one, but not one required by an
/// explicit `ask` rule.
pub async fn evaluate(
    policy: &Policy,
    hooks: &Hooks,
    tool: &str,
    spec: &ToolSpec,
    args: Value,
) -> GateOutcome {
    let pre = hooks.pre_tool_use(tool, &args).await;
    if pre.is_block() {
        return GateOutcome {
            verdict: GateVerdict::Deny(format!(
                "blocked by PreToolUse hook: {}",
                pre.block_reason()
            )),
            args,
        };
    }
    let args = pre.updated_input.unwrap_or(args);

    let verdict = match policy.check_call(tool, spec, &args) {
        PolicyDecision::Deny(reason) => GateVerdict::Deny(reason),
        PolicyDecision::AlwaysAsk => GateVerdict::Ask,
        PolicyDecision::Allow => match pre.decision {
            HookDecision::Ask => GateVerdict::Ask,
            _ => GateVerdict::Allow,
        },
        PolicyDecision::RequireApproval => match pre.decision {
            HookDecision::Allow => GateVerdict::Allow,
            _ => GateVerdict::Ask,
        },
    };
    GateOutcome { verdict, args }
}

#[cfg(test)]
mod tests {
    use super::super::hooks::{HookConfig, HookEvent};
    use super::*;
    use crate::harness::rules::Rules;
    use crate::tool::tool_trait::ToolCategory;
    use serde_json::json;

    fn spec() -> ToolSpec {
        ToolSpec::new("write", "d", json!({})).with_category(ToolCategory::Write)
    }

    fn hooks(command: &str) -> Hooks {
        Hooks::new(
            &[HookConfig {
                event: HookEvent::PreToolUse,
                matcher: None,
                command: command.into(),
                timeout_secs: Some(5),
                fail_closed: false,
            }],
            None,
        )
        .unwrap()
    }

    #[tokio::test]
    async fn hook_allow_skips_prompt_but_not_deny_or_ask_rules() {
        let allow = hooks(r#"echo {"decision":"allow"}"#);
        let args = json!({"path": "a.txt"});

        let policy = Policy::new(vec![]);
        let out = evaluate(&policy, &allow, "write", &spec(), args.clone()).await;
        assert_eq!(out.verdict, GateVerdict::Allow);

        let s = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let ask = Policy::new(vec![]).with_rules(Rules::parse(&[], &s(&["write"]), &[]).unwrap());
        let out = evaluate(&ask, &allow, "write", &spec(), args.clone()).await;
        assert_eq!(out.verdict, GateVerdict::Ask);

        let deny = Policy::new(vec![]).with_rules(Rules::parse(&s(&["write"]), &[], &[]).unwrap());
        let out = evaluate(&deny, &allow, "write", &spec(), args).await;
        assert!(matches!(out.verdict, GateVerdict::Deny(_)));
    }

    #[tokio::test]
    async fn hook_ask_forces_prompt_and_rewrites_args() {
        let h = hooks(r#"echo {"decision":"ask","updated_input":{"path":"b.txt"}}"#);
        let policy = Policy::new(vec!["write".into()]);
        let out = evaluate(&policy, &h, "write", &spec(), json!({"path": "a.txt"})).await;
        assert_eq!(out.verdict, GateVerdict::Ask);
        assert_eq!(out.args, json!({"path": "b.txt"}));
    }

    #[tokio::test]
    async fn rewritten_args_are_policy_checked() {
        let h = hooks(r#"echo {"updated_input":{"path":"secrets/k"}}"#);
        let s = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let policy = Policy::new(vec![])
            .with_rules(Rules::parse(&s(&["write:secrets/**"]), &[], &[]).unwrap());
        let out = evaluate(&policy, &h, "write", &spec(), json!({"path": "a"})).await;
        assert!(matches!(out.verdict, GateVerdict::Deny(_)));
    }

    #[tokio::test]
    async fn hook_block_denies() {
        let h = hooks("echo stop 1>&2 && exit 2");
        let out = evaluate(&Policy::default(), &h, "write", &spec(), json!({})).await;
        assert_eq!(
            out.verdict,
            GateVerdict::Deny("blocked by PreToolUse hook: stop".into())
        );
    }
}
