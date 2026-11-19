# Milestone 13 Implementation Plan

## Tasks

### 1. Add gix dependency to Cargo.toml
- Add `gix` as a dependency (pure Rust git implementation)
- Ensure it's included in the default features

### 2. Rewrite SnapshotStore to use gix instead of git CLI
- Replace all `Command::new("git")` calls with gix API
- Implement snapshot creation, restore, history loading using gix
- Maintain same public API (SnapshotEntry, SnapshotError, etc.)

### 3. Make git initialization opt-in/deferred
- Modify `SnapshotStore::new` to NOT initialize git repo automatically
- Add a config flag `snapshots.enabled` or CLI flag `--snapshots` to control
- Only initialize git when first snapshot is actually taken

### 4. Split session module into separate files
- `src/session/log.rs` - append-only JSONL logging
- `src/session/header.rs` - session header (id, cwd, version)
- `src/session/resume.rs` - resume store for `--resume`
- `src/session/mod.rs` - re-exports

### 5. Connect /undo and /redo commands to snapshot store
- Add undo/redo handling in TerminalApp's handle_command_action
- Send action to harness to perform undo/redo on snapshot store
- Update TUI state after undo/redo

### 6. Add graceful shutdown flush in TUI
- In `run_tui_and_loop`, call `session_manager.end_session()` on SIGINT/exit
- Ensure JSONL log is flushed and alternate screen torn down

### 7. Add snapshot config to CLI/config
- Add `[snapshots]` section to config with `enabled = true/false`
- Add `--snapshots` CLI flag

### 8. Update tests
- Ensure all tests pass with gix implementation
- Test undo/redo functionality end-to-end
- Test session log module restructure

## Dependencies
- gix crate (pure Rust git)

## Acceptance Criteria
- `/undo` restores prior working-tree state; `/redo` re-applies it
- Snapshots taken before any `write`/`delete`/destructive `run`
- Undo/redo cursor bounded; cannot undo past oldest snapshot
- Session log types (`SessionLog`, `SessionHeader`, `ResumeStore`) exist as separate modules per ARCHITECTURE.md
- SIGINT during a TUI turn flushes the session log and tears down the alternate screen before exit
- Launching the TUI or headless binary in a directory without `.git` does not create a `.git` folder unless snapshots are explicitly enabled or a snapshot is taken
- CI green (fmt, clippy, test)