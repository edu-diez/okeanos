//! Harness core (ReAct loop, context, ...).

pub mod error;
pub mod gate;
pub mod hooks;
pub mod policy;
pub mod react;
pub mod retry;
pub mod rules;
pub mod subagent;

pub use crate::snapshot::undo_redo::{UndoRedo, UndoRedoError};
pub use crate::snapshot::{SnapshotError, SnapshotStore};
pub use error::HarnessError;
pub use hooks::{HookConfig, HookDecision, HookEvent, Hooks, PreToolOutcome};
pub use policy::{ModeHandle, PermissionMode, Policy, PolicyDecision};
pub use react::{LoopDecision, ReactConfig, ReactError, ReactLoop};
pub use retry::{RetryDecision, RetryPolicy};
pub use rules::{Rule, Rules};
pub use subagent::{
    SpawnConfig, SpawnConfig as SubAgentSpawnConfig, SubAgentError,
    SubAgentError as SubAgentErrorType, SubAgentHandle, SubAgentHandle as SubAgentHandleType,
    SubAgentResult, SubAgentResult as SubAgentResultType, WorkspaceHandle,
    WorkspaceHandle as WorkspaceHandleType, spawn_subagent,
};
