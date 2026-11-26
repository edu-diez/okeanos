//! Tier-appropriate built-in tool sets.
//!
//! One place decides which tools each model tier sees, so every entry point
//! (TUI, headless, RPC, ...) registers an identical, deterministic set.

use crate::model::ModelTier;
use crate::tool::builtin::{
    DeleteTool, EditFormat, EditTool, GlobTool, GrepTool, ReadTool, RunTool, WriteTool,
};
use crate::tool::todo::{TodoList, TodoTool};
use crate::tool::tool_trait::ToolRegistry;

/// Build the built-in registry for `tier`.
///
/// - Small: `read`, `edit`, `run` (minimal schema surface).
/// - Mid/Frontier: adds `todo`, `write`, `delete`, `grep`, `glob`; `edit`
///   is available everywhere so targeted changes never need a full rewrite.
pub fn builtin_registry(
    tier: ModelTier,
    edit_format: EditFormat,
    todo_list: &TodoList,
) -> ToolRegistry {
    let mut tools = ToolRegistry::new();
    tools.register(ReadTool::new());
    tools.register(EditTool::with_format(edit_format));
    tools.register(RunTool::new());
    if tier != ModelTier::Small {
        tools.register(TodoTool::new(todo_list.clone()));
        tools.register(WriteTool::new());
        tools.register(DeleteTool::new());
        tools.register(GrepTool::new());
        tools.register(GlobTool::new());
    }
    tools
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(tier: ModelTier) -> Vec<String> {
        builtin_registry(tier, EditFormat::StrReplace, &TodoList::new())
            .specs()
            .into_iter()
            .map(|s| s.name)
            .collect()
    }

    #[test]
    fn small_tier_is_minimal() {
        assert_eq!(names(ModelTier::Small), ["edit", "read", "run"]);
    }

    #[test]
    fn larger_tiers_get_edit_and_search() {
        for tier in [ModelTier::Mid, ModelTier::Frontier] {
            let n = names(tier);
            for t in ["edit", "write", "grep", "glob", "todo"] {
                assert!(n.iter().any(|x| x == t), "{tier:?} missing {t}");
            }
        }
    }
}
