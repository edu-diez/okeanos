//! Git snapshots and undo/redo functionality.
//!
//! Provides gix-based snapshot store for creating git snapshots before
//! destructive actions and on loop boundaries, with undo/redo cursor support.

pub mod store;
pub mod undo_redo;

pub use store::{FileRestore, SnapshotError, SnapshotStore};
pub use undo_redo::{UndoRedo, UndoRedoError};
