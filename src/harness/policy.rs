//! Approval policy wiring (stub for Milestone 2).
//!
//! The full permission layer is implemented in Milestone 5.
//! This stub allows the ReAct loop to compile and run without approval checks.

use crate::events::ApprovalDecision;
use crate::tool::ToolSpec;
use std::collections::HashSet;

/// Policy decision for a tool call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicyDecision {
    Allow,
    Deny,
    RequireApproval,
}

/// Simple allow-list policy (stub).
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

    /// Check if a tool call is allowed.
    pub fn check(&self, tool: &str, _spec: &ToolSpec) -> PolicyDecision {
        if self.allow_list.contains(tool) || self.session_allows.contains(tool) {
            PolicyDecision::Allow
        } else {
            PolicyDecision::RequireApproval
        }
    }

    /// Grant allow-always for a tool in this session.
    pub fn allow_always(&mut self, tool: &str) {
        self.session_allows.insert(tool.to_string());
    }

    /// Record an approval decision.
    pub fn record_decision(&mut self, tool: &str, decision: ApprovalDecision) {
        if decision == ApprovalDecision::AllowAlways {
            self.allow_always(tool);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::ToolSpec;
    use serde_json::json;

    #[test]
    fn test_policy_allow_list() {
        let policy = Policy::new(vec!["todo".into(), "read".into()]);
        let spec = ToolSpec::new("todo", "desc", json!({}));
        assert_eq!(policy.check("todo", &spec), PolicyDecision::Allow);
        assert_eq!(policy.check("read", &spec), PolicyDecision::Allow);
        assert_eq!(
            policy.check("write", &spec),
            PolicyDecision::RequireApproval
        );
    }

    #[test]
    fn test_policy_session_allow_always() {
        let mut policy = Policy::new(vec!["todo".into()]);
        let spec = ToolSpec::new("write", "desc", json!({}));

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
        let spec = ToolSpec::new("write", "desc", json!({}));

        policy.record_decision("write", ApprovalDecision::Allow);
        assert_eq!(
            policy.check("write", &spec),
            PolicyDecision::RequireApproval
        );

        policy.record_decision("write", ApprovalDecision::AllowAlways);
        assert_eq!(policy.check("write", &spec), PolicyDecision::Allow);
    }
}
