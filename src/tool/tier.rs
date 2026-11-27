//! Tier-appropriate built-in tool sets.
//!
//! One place decides which tools each model tier sees, so every entry point
//! (TUI, headless, RPC, ...) registers an identical, deterministic set.

use crate::model::ModelTier;
use crate::tool::builtin::{
    DeleteTool, EditFormat, EditTool, GlobTool, GrepTool, JobKillTool, JobOutputTool, ReadTool,
    RunTool, WriteTool,
};
use crate::tool::todo::{TodoList, TodoTool};
use crate::tool::tool_trait::ToolRegistry;

/// Build the built-in registry for `tier`.
///
/// - Small: `read`, `edit` (single-format schema), `run`, plus the read-only search tools
///   `grep` and `glob` (so searching never needs an approval-gated `run`).
/// - Mid/Frontier: adds `todo`, `write`, `delete`, `job_output`, `job_kill`; `edit`
///   is available everywhere so targeted changes never need a full rewrite.
pub fn builtin_registry(
    tier: ModelTier,
    edit_format: EditFormat,
    todo_list: &TodoList,
) -> ToolRegistry {
    let mut tools = ToolRegistry::new();
    tools.register(ReadTool::new());
    let edit = EditTool::with_format(edit_format);
    tools.register(if tier == ModelTier::Small {
        edit.simplified()
    } else {
        edit
    });
    tools.register(RunTool::new());
    tools.register(GrepTool::new());
    tools.register(GlobTool::new());
    if tier != ModelTier::Small {
        tools.register(TodoTool::new(todo_list.clone()));
        tools.register(WriteTool::new());
        tools.register(DeleteTool::new());
        tools.register(JobOutputTool::new());
        tools.register(JobKillTool::new());
    }
    tools
}

/// Shrink `tools` in place after a mid-session downgrade to `tier`, so the
/// registry matches what [`builtin_registry`] would have produced. Never adds
/// tools a larger tier would have.
pub fn downgrade_registry(tools: &mut ToolRegistry, tier: ModelTier, edit_format: EditFormat) {
    if tier != ModelTier::Small {
        return;
    }
    for name in ["todo", "write", "delete", "job_output", "job_kill"] {
        tools.remove(name);
    }
    tools.register(EditTool::with_format(edit_format).simplified());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn downgrade_matches_small_registry() {
        let mut reg = builtin_registry(ModelTier::Mid, EditFormat::Diff, &TodoList::new());
        downgrade_registry(&mut reg, ModelTier::Small, EditFormat::StrReplace);
        let mut got: Vec<String> = reg.specs().into_iter().map(|s| s.name).collect();
        got.sort();
        assert_eq!(got, ["edit", "glob", "grep", "read", "run"]);
        let edit = reg.specs().into_iter().find(|s| s.name == "edit").unwrap();
        assert!(edit.input_schema.get("oneOf").is_none());
    }

    fn names(tier: ModelTier) -> Vec<String> {
        builtin_registry(tier, EditFormat::StrReplace, &TodoList::new())
            .specs()
            .into_iter()
            .map(|s| s.name)
            .collect()
    }

    #[test]
    fn small_tier_is_minimal() {
        assert_eq!(
            names(ModelTier::Small),
            ["edit", "glob", "grep", "read", "run"]
        );
    }

    #[test]
    fn small_tier_edit_schema_is_simplified() {
        let reg = builtin_registry(ModelTier::Small, EditFormat::StrReplace, &TodoList::new());
        let edit = reg.specs().into_iter().find(|s| s.name == "edit").unwrap();
        assert!(edit.input_schema.get("oneOf").is_none());
    }

    #[test]
    fn larger_tiers_get_edit_and_search() {
        for tier in [ModelTier::Mid, ModelTier::Frontier] {
            let n = names(tier);
            for t in [
                "edit",
                "write",
                "grep",
                "glob",
                "todo",
                "job_output",
                "job_kill",
            ] {
                assert!(n.iter().any(|x| x == t), "{tier:?} missing {t}");
            }
        }
    }
}
