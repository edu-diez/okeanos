//! Approval policy wiring (stub for Milestone 2).
//!
//! The full permission layer is implemented in Milestone 5.
//! This stub allows the ReAct loop to compile and run without approval checks.

use crate::events::ApprovalDecision;
use crate::tool::ToolSpec;
use crate::tool::tool_trait::ToolCategory;
use std::collections::HashSet;

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

    /// Grant allow-always for a tool in this session.
    pub fn allow_always(&mut self, tool: &str) {
        self.session_allows.insert(tool.to_string());
    }

    /// Record an approval decision, persisting allow-always grants.
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
}
