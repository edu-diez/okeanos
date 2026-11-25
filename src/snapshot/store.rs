//! Git snapshots using gix (pure Rust).
//!
//! Provides snapshot functionality using the gix crate for pure-Rust
//! git operations without shelling out to the git CLI. No C dependencies.

use crate::events::Event;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};
use thiserror::Error;

/// Errors related to snapshot operations.
#[derive(Debug, Error)]
pub enum SnapshotError {
    #[error("failed to initialize repository: {source}")]
    InitRepo { source: std::io::Error },
    #[error("repository not found at {path}: {source}")]
    RepoNotFound {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to create snapshot: {message}")]
    CreateSnapshot { message: String },
    #[error("failed to restore snapshot: {message}")]
    RestoreSnapshot { message: String },
    #[error("no snapshots available")]
    NoSnapshots,
    #[error("invalid snapshot reference: {reference}")]
    InvalidRef { reference: String },
    #[error("working tree has uncommitted changes; commit or stash first")]
    DirtyWorktree,
    #[error("failed to get status: {message}")]
    Status { message: String },
    #[error("IO error: {source}")]
    Io { source: std::io::Error },
    #[error("snapshot index out of bounds: {index} (max: {max})")]
    OutOfBounds { index: usize, max: usize },
    #[error("git error: {0}")]
    Git(#[from] std::io::Error),
}

/// A snapshot entry in the store.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SnapshotEntry {
    /// Unique snapshot identifier (commit hash).
    pub id: String,
    /// Human-readable description.
    pub description: String,
    /// Unix timestamp in milliseconds when snapshot was created.
    pub timestamp: u64,
    /// The event index in the session log when this snapshot was taken.
    pub event_index: usize,
}

/// Snapshot store using git CLI for git operations.
///
/// Snapshots are implemented as commits on a special ref `refs/okeanos/snapshots/HEAD`
/// which points to the current snapshot position. The undo/redo cursor moves along
/// this history.
#[derive(Debug, Clone)]
pub struct SnapshotStore {
    /// Path to the git repository (project root).
    repo_path: PathBuf,
    /// Current snapshot cursor index (0 = oldest, len-1 = newest).
    cursor: usize,
    /// List of snapshot entries in chronological order.
    snapshots: Vec<SnapshotEntry>,
    /// The branch/ref name used for snapshots.
    snapshot_ref: String,
    /// Whether the git repo has been initialized for snapshots.
    initialized: bool,
}

impl SnapshotStore {
    /// Create a new snapshot store for the given project root.
    ///
    /// Does NOT initialize a git repository. Git initialization is deferred
    /// until the first snapshot is actually taken, or when snapshots are
    /// explicitly enabled via config/CLI.
    pub fn new(project_root: &Path) -> Result<Self, SnapshotError> {
        let repo_path = project_root.to_path_buf();
        let snapshot_ref = "refs/okeanos/snapshots/HEAD".to_string();

        let mut store = Self {
            repo_path,
            cursor: 0,
            snapshots: Vec::new(),
            snapshot_ref,
            initialized: false,
        };

        // Try to load existing snapshot history if repo already exists
        if store.repo_path.join(".git").exists() {
            store.initialized = true;
            store.load_snapshot_history()?;
        }

        Ok(store)
    }

    /// Ensure the git repository is initialized for snapshots.
    ///
    /// This is called lazily when the first snapshot is taken.
    fn ensure_initialized(&mut self) -> Result<(), SnapshotError> {
        if self.initialized {
            return Ok(());
        }

        // Initialize git repository
        let output = Command::new("git")
            .args(["init"])
            .current_dir(&self.repo_path)
            .output()
            .map_err(|source| SnapshotError::InitRepo { source })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(SnapshotError::InitRepo {
                source: std::io::Error::other(format!("git init failed: {}", stderr)),
            });
        }

        // Configure user for commits (required for commit-tree)
        Command::new("git")
            .args(["config", "user.email", "okeanos@local"])
            .current_dir(&self.repo_path)
            .output()
            .ok();
        Command::new("git")
            .args(["config", "user.name", "okeanos"])
            .current_dir(&self.repo_path)
            .output()
            .ok();

        self.initialized = true;
        Ok(())
    }

    /// Create a new snapshot with the given description.
    ///
    /// Takes a snapshot of the current working tree state by committing all
    /// changes to the snapshot ref. Returns the snapshot ID (commit hash).
    pub fn snapshot(
        &mut self,
        description: &str,
        event_index: usize,
    ) -> Result<String, SnapshotError> {
        // Ensure git is initialized
        self.ensure_initialized()?;

        // Add all changes to the index
        let output = Command::new("git")
            .args(["add", "-A"])
            .current_dir(&self.repo_path)
            .output()
            .map_err(|source| SnapshotError::CreateSnapshot {
                message: format!("git add failed: {}", source),
            })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(SnapshotError::CreateSnapshot {
                message: format!("git add failed: {}", stderr),
            });
        }

        // Write tree
        let output = Command::new("git")
            .args(["write-tree"])
            .current_dir(&self.repo_path)
            .output()
            .map_err(|source| SnapshotError::CreateSnapshot {
                message: format!("git write-tree failed: {}", source),
            })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(SnapshotError::CreateSnapshot {
                message: format!("git write-tree failed: {}", stderr),
            });
        }

        let tree_id = String::from_utf8_lossy(&output.stdout).trim().to_string();

        // Get parent commit from snapshot ref if it exists
        let parent = self.get_snapshot_ref_commit()?;

        // Create commit
        let timestamp = current_timestamp();
        let commit_message = format!(
            "okeanos snapshot: {}\n\nEvent index: {}\nTimestamp: {}",
            description, event_index, timestamp
        );

        let mut commit_args: Vec<String> = vec![
            "commit-tree".to_string(),
            tree_id.clone(),
            "-m".to_string(),
            commit_message.clone(),
        ];
        if let Some(parent_id) = parent {
            commit_args.push("-p".to_string());
            commit_args.push(parent_id);
        }

        let commit_args_str: Vec<&str> = commit_args.iter().map(|s| s.as_str()).collect();

        let output = Command::new("git")
            .args(&commit_args_str)
            .current_dir(&self.repo_path)
            .output()
            .map_err(|source| SnapshotError::CreateSnapshot {
                message: format!("git commit-tree failed: {}", source),
            })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(SnapshotError::CreateSnapshot {
                message: format!("git commit-tree failed: {}", stderr),
            });
        }

        let commit_id = String::from_utf8_lossy(&output.stdout).trim().to_string();

        // Update snapshot ref
        let output = Command::new("git")
            .args(["update-ref", &self.snapshot_ref, &commit_id])
            .current_dir(&self.repo_path)
            .output()
            .map_err(|source| SnapshotError::CreateSnapshot {
                message: format!("git update-ref failed: {}", source),
            })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(SnapshotError::CreateSnapshot {
                message: format!("git update-ref failed: {}", stderr),
            });
        }

        let entry = SnapshotEntry {
            id: commit_id.clone(),
            description: description.to_string(),
            timestamp,
            event_index,
        };

        // Add to snapshots list and move cursor to the new snapshot
        self.snapshots.push(entry);
        self.cursor = self.snapshots.len().saturating_sub(1);

        Ok(commit_id)
    }

    /// Restore the working tree to the snapshot at the given index.
    ///
    /// This performs a hard reset to the specified snapshot commit.
    pub fn restore(&mut self, index: usize) -> Result<(), SnapshotError> {
        if index >= self.snapshots.len() {
            return Err(SnapshotError::OutOfBounds {
                index,
                max: self.snapshots.len().saturating_sub(1),
            });
        }

        let snapshot = &self.snapshots[index];

        // Hard reset the worktree to the snapshot's tree
        let output = Command::new("git")
            .args(["reset", "--hard", &snapshot.id])
            .current_dir(&self.repo_path)
            .output()
            .map_err(|source| SnapshotError::RestoreSnapshot {
                message: format!("git reset failed: {}", source),
            })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(SnapshotError::RestoreSnapshot {
                message: format!("git reset failed: {}", stderr),
            });
        }

        // Update cursor
        self.cursor = index;

        Ok(())
    }

    /// Get the current cursor position.
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Get the total number of snapshots.
    pub fn len(&self) -> usize {
        self.snapshots.len()
    }

    /// Check if there are any snapshots.
    pub fn is_empty(&self) -> bool {
        self.snapshots.is_empty()
    }

    /// Get all snapshot entries.
    pub fn snapshots(&self) -> &[SnapshotEntry] {
        &self.snapshots
    }

    /// Check if we can undo (move cursor back).
    pub fn can_undo(&self) -> bool {
        self.cursor > 0
    }

    /// Check if we can redo (move cursor forward).
    pub fn can_redo(&self) -> bool {
        self.cursor + 1 < self.snapshots.len()
    }

    /// Move cursor back (undo) and restore the previous snapshot.
    pub fn undo(&mut self) -> Result<SnapshotEntry, SnapshotError> {
        if !self.can_undo() {
            return Err(SnapshotError::NoSnapshots);
        }

        let new_index = self.cursor - 1;
        self.restore(new_index)?;

        Ok(self.snapshots[new_index].clone())
    }

    /// Move cursor forward (redo) and restore the next snapshot.
    pub fn redo(&mut self) -> Result<SnapshotEntry, SnapshotError> {
        if !self.can_redo() {
            return Err(SnapshotError::NoSnapshots);
        }

        let new_index = self.cursor + 1;
        self.restore(new_index)?;

        Ok(self.snapshots[new_index].clone())
    }

    /// Get the current snapshot entry.
    pub fn current(&self) -> Option<&SnapshotEntry> {
        self.snapshots.get(self.cursor)
    }

    /// Load existing snapshot history from the snapshot ref.
    fn load_snapshot_history(&mut self) -> Result<(), SnapshotError> {
        // Check if .git exists
        if !self.repo_path.join(".git").exists() {
            return Ok(());
        }

        // Get the commit at the snapshot ref
        let output = Command::new("git")
            .args(["rev-parse", &self.snapshot_ref])
            .current_dir(&self.repo_path)
            .output()
            .map_err(SnapshotError::Git)?;

        if !output.status.success() {
            // No snapshot ref yet, that's fine
            return Ok(());
        }

        let commit_id = String::from_utf8_lossy(&output.stdout).trim().to_string();

        // Walk the commit history from the snapshot ref using --oneline format
        let output = Command::new("git")
            .args(["log", "--oneline", &commit_id])
            .current_dir(&self.repo_path)
            .output()
            .map_err(SnapshotError::Git)?;

        if !output.status.success() {
            return Ok(());
        }

        let log_output = String::from_utf8_lossy(&output.stdout);

        // Parse commits from --oneline format: "<hash> <title>"
        for line in log_output.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }

            // Format: "<hash> okeanos snapshot: <description>"
            let parts: Vec<&str> = line.splitn(2, ' ').collect();
            if parts.len() != 2 {
                continue;
            }

            let hash = parts[0];
            let title = parts[1];

            if !title.starts_with("okeanos snapshot: ") {
                continue;
            }

            let description = title
                .strip_prefix("okeanos snapshot: ")
                .unwrap_or("")
                .to_string();

            // For now, we don't have event_index and timestamp from --oneline
            // We'll use defaults
            let entry = SnapshotEntry {
                id: hash.to_string(),
                description,
                timestamp: 0,
                event_index: 0,
            };

            self.snapshots.push(entry);
        }

        // Reverse to get chronological order (oldest first)
        self.snapshots.reverse();

        // Set cursor to the latest snapshot
        self.cursor = self.snapshots.len().saturating_sub(1);

        Ok(())
    }

    /// Get the commit hash at the snapshot ref, if it exists.
    fn get_snapshot_ref_commit(&self) -> Result<Option<String>, SnapshotError> {
        let output = Command::new("git")
            .args(["rev-parse", &self.snapshot_ref])
            .current_dir(&self.repo_path)
            .output()
            .map_err(SnapshotError::Git)?;

        if output.status.success() {
            Ok(Some(
                String::from_utf8_lossy(&output.stdout).trim().to_string(),
            ))
        } else {
            Ok(None)
        }
    }

    /// Emit a snapshot progress event.
    pub fn emit_progress(
        &self,
        phase: &str,
        percent: f32,
        elapsed_ms: u64,
        event_tx: &tokio::sync::broadcast::Sender<Event>,
    ) {
        let progress = crate::events::SnapshotProgress {
            phase: phase.to_string(),
            percent,
            elapsed_ms,
        };
        let _ = event_tx.send(Event::SnapshotProgress(progress));
    }

    /// Check if the store has been initialized (git repo exists).
    pub fn is_initialized(&self) -> bool {
        self.initialized
    }
}

/// Get current Unix timestamp in milliseconds.
fn current_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_snapshot_store_new_no_git() {
        let dir = tempdir().unwrap();
        let store = SnapshotStore::new(dir.path()).unwrap();
        assert!(store.is_empty());
        assert_eq!(store.cursor(), 0);
        assert!(!store.can_undo());
        assert!(!store.can_redo());
        assert!(!store.is_initialized());
    }

    #[test]
    fn test_snapshot_store_new_existing_repo() {
        let dir = tempdir().unwrap();
        // Initialize a git repo first
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

        // Create an initial commit
        std::fs::write(dir.path().join("README.md"), "# Test\n").unwrap();
        std::process::Command::new("git")
            .args(["add", "README.md"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["commit", "-m", "Initial commit"])
            .current_dir(dir.path())
            .output()
            .unwrap();

        let store = SnapshotStore::new(dir.path()).unwrap();
        assert!(store.is_empty()); // No okeanos snapshots yet
        assert!(store.is_initialized());
    }

    #[test]
    fn test_snapshot_store_create_and_restore() {
        let dir = tempdir().unwrap();
        let mut store = SnapshotStore::new(dir.path()).unwrap();

        // Create a file and snapshot
        std::fs::write(dir.path().join("file1.txt"), "content 1").unwrap();
        let snap1 = store.snapshot("First snapshot", 0).unwrap();
        assert_eq!(store.len(), 1);
        assert_eq!(store.cursor(), 0);
        assert!(!store.can_undo());
        assert!(!store.can_redo());
        assert!(store.is_initialized());

        // Modify file and create second snapshot
        std::fs::write(dir.path().join("file1.txt"), "content 2").unwrap();
        let snap2 = store.snapshot("Second snapshot", 1).unwrap();
        assert_eq!(store.len(), 2);
        assert_eq!(store.cursor(), 1);
        assert!(store.can_undo());
        assert!(!store.can_redo());

        // Modify again for third snapshot
        std::fs::write(dir.path().join("file1.txt"), "content 3").unwrap();
        let _snap3 = store.snapshot("Third snapshot", 2).unwrap();
        assert_eq!(store.len(), 3);
        assert_eq!(store.cursor(), 2);
        assert!(store.can_undo());
        assert!(!store.can_redo());

        // Undo to second snapshot
        let restored = store.undo().unwrap();
        assert_eq!(restored.id, snap2);
        assert_eq!(store.cursor(), 1);
        let content = std::fs::read_to_string(dir.path().join("file1.txt")).unwrap();
        assert_eq!(content, "content 2");

        // Undo to first snapshot
        let restored = store.undo().unwrap();
        assert_eq!(restored.id, snap1);
        assert_eq!(store.cursor(), 0);
        let content = std::fs::read_to_string(dir.path().join("file1.txt")).unwrap();
        assert_eq!(content, "content 1");

        // Can't undo past first
        assert!(!store.can_undo());
        assert!(store.undo().is_err());

        // Redo to second
        let restored = store.redo().unwrap();
        assert_eq!(restored.id, snap2);
        assert_eq!(store.cursor(), 1);
        let content = std::fs::read_to_string(dir.path().join("file1.txt")).unwrap();
        assert_eq!(content, "content 2");

        // Redo to third
        let _restored = store.redo().unwrap();
        assert_eq!(store.cursor(), 2);
        let content = std::fs::read_to_string(dir.path().join("file1.txt")).unwrap();
        assert_eq!(content, "content 3");

        // Can't redo past last
        assert!(!store.can_redo());
        assert!(store.redo().is_err());
    }

    #[test]
    fn test_snapshot_store_restore_by_index() {
        let dir = tempdir().unwrap();
        let mut store = SnapshotStore::new(dir.path()).unwrap();

        std::fs::write(dir.path().join("file.txt"), "v1").unwrap();
        store.snapshot("v1", 0).unwrap();

        std::fs::write(dir.path().join("file.txt"), "v2").unwrap();
        store.snapshot("v2", 1).unwrap();

        std::fs::write(dir.path().join("file.txt"), "v3").unwrap();
        store.snapshot("v3", 2).unwrap();

        // Restore to index 0 (v1)
        store.restore(0).unwrap();
        let content = std::fs::read_to_string(dir.path().join("file.txt")).unwrap();
        assert_eq!(content, "v1");
        assert_eq!(store.cursor(), 0);

        // Restore to index 2 (v3)
        store.restore(2).unwrap();
        let content = std::fs::read_to_string(dir.path().join("file.txt")).unwrap();
        assert_eq!(content, "v3");
        assert_eq!(store.cursor(), 2);
    }

    #[test]
    fn test_snapshot_store_out_of_bounds() {
        let dir = tempdir().unwrap();
        let mut store = SnapshotStore::new(dir.path()).unwrap();
        std::fs::write(dir.path().join("file.txt"), "v1").unwrap();
        store.snapshot("v1", 0).unwrap();

        // Try to restore invalid index
        let err = store.restore(5).unwrap_err();
        assert!(matches!(err, SnapshotError::OutOfBounds { .. }));
    }

    #[test]
    fn test_snapshot_store_persists_across_instances() {
        let dir = tempdir().unwrap();
        {
            let mut store = SnapshotStore::new(dir.path()).unwrap();
            std::fs::write(dir.path().join("file.txt"), "v1").unwrap();
            store.snapshot("v1", 0).unwrap();
            std::fs::write(dir.path().join("file.txt"), "v2").unwrap();
            store.snapshot("v2", 1).unwrap();
        }

        // Create new store instance - should load existing snapshots
        let store = SnapshotStore::new(dir.path()).unwrap();
        assert_eq!(store.len(), 2);
        assert_eq!(store.cursor(), 1);
        assert!(store.can_undo());
        assert!(!store.can_redo());

        let content = std::fs::read_to_string(dir.path().join("file.txt")).unwrap();
        assert_eq!(content, "v2");
    }
}
