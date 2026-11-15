//! Undo/redo cursor management for snapshot navigation.

use crate::events::Event;
use crate::snapshot::store::{SnapshotError, SnapshotStore};
use thiserror::Error;
use tokio::sync::broadcast;

/// Errors related to undo/redo operations.
#[derive(Debug, Error)]
pub enum UndoRedoError {
    #[error("snapshot error: {0}")]
    Snapshot(#[from] SnapshotError),
    #[error("cannot undo: at oldest snapshot")]
    CannotUndo,
    #[error("cannot redo: at newest snapshot")]
    CannotRedo,
    #[error("no snapshots available")]
    NoSnapshots,
}

/// Manages undo/redo operations on a snapshot store.
///
/// Provides a high-level interface for navigating snapshot history,
/// including emitting progress events for UI integration.
#[derive(Debug)]
pub struct UndoRedo {
    store: SnapshotStore,
    event_tx: broadcast::Sender<Event>,
}

impl UndoRedo {
    /// Create a new undo/redo manager.
    pub fn new(store: SnapshotStore, event_tx: broadcast::Sender<Event>) -> Self {
        Self { store, event_tx }
    }

    /// Get the underlying snapshot store.
    pub fn store(&self) -> &SnapshotStore {
        &self.store
    }

    /// Get a mutable reference to the underlying snapshot store.
    pub fn store_mut(&mut self) -> &mut SnapshotStore {
        &mut self.store
    }

    /// Create a new snapshot with the given description.
    ///
    /// This is called before destructive actions and on loop boundaries.
    pub fn snapshot(
        &mut self,
        description: &str,
        event_index: usize,
    ) -> Result<String, UndoRedoError> {
        self.store
            .emit_progress("Creating snapshot", 0.0, 0, &self.event_tx);
        let id = self.store.snapshot(description, event_index)?;
        self.store
            .emit_progress("Snapshot created", 100.0, 0, &self.event_tx);
        Ok(id)
    }

    /// Undo to the previous snapshot.
    ///
    /// Returns the restored snapshot entry on success.
    pub fn undo(&mut self) -> Result<crate::snapshot::store::SnapshotEntry, UndoRedoError> {
        if !self.store.can_undo() {
            return Err(UndoRedoError::CannotUndo);
        }

        self.store.emit_progress("Undoing", 0.0, 0, &self.event_tx);
        let entry = self.store.undo()?;
        self.store
            .emit_progress("Undo complete", 100.0, 0, &self.event_tx);
        Ok(entry)
    }

    /// Redo to the next snapshot.
    ///
    /// Returns the restored snapshot entry on success.
    pub fn redo(&mut self) -> Result<crate::snapshot::store::SnapshotEntry, UndoRedoError> {
        if !self.store.can_redo() {
            return Err(UndoRedoError::CannotRedo);
        }

        self.store.emit_progress("Redoing", 0.0, 0, &self.event_tx);
        let entry = self.store.redo()?;
        self.store
            .emit_progress("Redo complete", 100.0, 0, &self.event_tx);
        Ok(entry)
    }

    /// Check if undo is available.
    pub fn can_undo(&self) -> bool {
        self.store.can_undo()
    }

    /// Check if redo is available.
    pub fn can_redo(&self) -> bool {
        self.store.can_redo()
    }

    /// Get the current cursor position.
    pub fn cursor(&self) -> usize {
        self.store.cursor()
    }

    /// Get the total number of snapshots.
    pub fn len(&self) -> usize {
        self.store.len()
    }

    /// Check if there are any snapshots.
    pub fn is_empty(&self) -> bool {
        self.store.is_empty()
    }

    /// Get all snapshot entries.
    pub fn snapshots(&self) -> &[crate::snapshot::store::SnapshotEntry] {
        self.store.snapshots()
    }

    /// Get the current snapshot entry.
    pub fn current(&self) -> Option<&crate::snapshot::store::SnapshotEntry> {
        self.store.current()
    }

    /// Restore to a specific snapshot index.
    pub fn restore(&mut self, index: usize) -> Result<(), UndoRedoError> {
        self.store
            .emit_progress("Restoring snapshot", 0.0, 0, &self.event_tx);
        self.store.restore(index)?;
        self.store
            .emit_progress("Restore complete", 100.0, 0, &self.event_tx);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::event_channel;
    use tempfile::tempdir;

    #[tokio::test]
    async fn test_undo_redo_basic() {
        let dir = tempdir().unwrap();
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(dir.path())
            .output()
            .unwrap();

        let (event_tx, _event_rx) = event_channel();
        let store = SnapshotStore::new(dir.path()).unwrap();
        let mut undo_redo = UndoRedo::new(store, event_tx);

        assert!(!undo_redo.can_undo());
        assert!(!undo_redo.can_redo());

        // Create first snapshot
        std::fs::write(dir.path().join("file.txt"), "v1").unwrap();
        undo_redo.snapshot("v1", 0).unwrap();
        assert_eq!(undo_redo.len(), 1);
        assert!(!undo_redo.can_undo());
        assert!(!undo_redo.can_redo());

        // Create second snapshot
        std::fs::write(dir.path().join("file.txt"), "v2").unwrap();
        undo_redo.snapshot("v2", 1).unwrap();
        assert_eq!(undo_redo.len(), 2);
        assert!(undo_redo.can_undo());
        assert!(!undo_redo.can_redo());

        // Create third snapshot
        std::fs::write(dir.path().join("file.txt"), "v3").unwrap();
        undo_redo.snapshot("v3", 2).unwrap();
        assert_eq!(undo_redo.len(), 3);
        assert!(undo_redo.can_undo());
        assert!(!undo_redo.can_redo());

        // Undo to v2
        let entry = undo_redo.undo().unwrap();
        assert_eq!(entry.description, "v2");
        assert_eq!(undo_redo.cursor(), 1);
        assert!(undo_redo.can_undo());
        assert!(undo_redo.can_redo());

        // Undo to v1
        let entry = undo_redo.undo().unwrap();
        assert_eq!(entry.description, "v1");
        assert_eq!(undo_redo.cursor(), 0);
        assert!(!undo_redo.can_undo());
        assert!(undo_redo.can_redo());

        // Can't undo further
        assert!(matches!(undo_redo.undo(), Err(UndoRedoError::CannotUndo)));

        // Redo to v2
        let entry = undo_redo.redo().unwrap();
        assert_eq!(entry.description, "v2");
        assert_eq!(undo_redo.cursor(), 1);

        // Redo to v3
        let entry = undo_redo.redo().unwrap();
        assert_eq!(entry.description, "v3");
        assert_eq!(undo_redo.cursor(), 2);
        assert!(undo_redo.can_undo());
        assert!(!undo_redo.can_redo());

        // Can't redo further
        assert!(matches!(undo_redo.redo(), Err(UndoRedoError::CannotRedo)));
    }

    #[tokio::test]
    async fn test_undo_redo_restore_by_index() {
        let dir = tempdir().unwrap();
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(dir.path())
            .output()
            .unwrap();

        let (event_tx, _event_rx) = event_channel();
        let store = SnapshotStore::new(dir.path()).unwrap();
        let mut undo_redo = UndoRedo::new(store, event_tx);

        std::fs::write(dir.path().join("file.txt"), "v1").unwrap();
        undo_redo.snapshot("v1", 0).unwrap();

        std::fs::write(dir.path().join("file.txt"), "v2").unwrap();
        undo_redo.snapshot("v2", 1).unwrap();

        std::fs::write(dir.path().join("file.txt"), "v3").unwrap();
        undo_redo.snapshot("v3", 2).unwrap();

        // Restore to index 0
        undo_redo.restore(0).unwrap();
        assert_eq!(undo_redo.cursor(), 0);
        let content = std::fs::read_to_string(dir.path().join("file.txt")).unwrap();
        assert_eq!(content, "v1");

        // Restore to index 2
        undo_redo.restore(2).unwrap();
        assert_eq!(undo_redo.cursor(), 2);
        let content = std::fs::read_to_string(dir.path().join("file.txt")).unwrap();
        assert_eq!(content, "v3");
    }

    #[tokio::test]
    async fn test_undo_redo_progress_events() {
        let dir = tempdir().unwrap();
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(dir.path())
            .output()
            .unwrap();

        let (event_tx, mut event_rx) = event_channel();
        let store = SnapshotStore::new(dir.path()).unwrap();
        let mut undo_redo = UndoRedo::new(store, event_tx);

        std::fs::write(dir.path().join("file.txt"), "v1").unwrap();
        undo_redo.snapshot("v1", 0).unwrap();

        // Check for progress events
        let mut snapshot_created = false;
        for _ in 0..10 {
            if let Ok(event) = event_rx.try_recv()
                && let crate::events::Event::SnapshotProgress(sp) = event
                && sp.phase == "Snapshot created"
                && sp.percent == 100.0
            {
                snapshot_created = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(snapshot_created, "expected SnapshotProgress event");
    }
}
