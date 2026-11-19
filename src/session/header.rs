//! Session header (first line of JSONL file).

use crate::events::Event;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use thiserror::Error;

/// Current session log format version.
pub const SESSION_FORMAT_VERSION: u32 = 1;

/// Errors related to session management.
#[derive(Debug, Error)]
pub enum SessionError {
    #[error("failed to create session directory {path}: {source}")]
    CreateDir {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to open session log {path}: {source}")]
    OpenLog {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to write session log {path}: {source}")]
    WriteLog {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to read session log {path}: {source}")]
    ReadLog {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("invalid session header: {message}")]
    InvalidHeader { message: String },
    #[error("session not found: {id}")]
    NotFound { id: String },
    #[error("corrupted line at line {line}: {message}")]
    CorruptedLine { line: usize, message: String },
    #[error("failed to parse event: {source}")]
    ParseEvent { source: serde_json::Error },
    #[error("failed to serialize event: {source}")]
    SerializeEvent { source: serde_json::Error },
    #[error("no cwd available for session storage")]
    NoCwd,
}

/// Session header (first line of JSONL file).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionHeader {
    /// Unique session identifier.
    pub id: String,
    /// Working directory when session started.
    pub cwd: String,
    /// Session log format version.
    pub format_version: u32,
    /// Session start timestamp (Unix ms).
    pub started_at: u64,
    /// Project root directory (git root or CWD).
    pub project_root: String,
}

/// A logged event with metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionEntry {
    /// Unix timestamp in milliseconds.
    pub ts: u64,
    /// The event payload.
    pub event: Event,
}

/// Session metadata for listing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    pub id: String,
    pub cwd: String,
    pub project_root: String,
    pub started_at: u64,
    pub last_event_at: u64,
    pub event_count: usize,
}

/// Generate a unique session ID.
pub fn generate_session_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let random: u32 = rand::random();
    format!("session-{:x}-{:x}", timestamp, random)
}

/// Get current Unix timestamp in milliseconds.
pub fn current_timestamp() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
