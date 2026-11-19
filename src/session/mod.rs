//! Session log and resume functionality.
//!
//! Provides append-only JSONL session logging with project-scoped storage,
//! resume capability, and corrupted line tolerance.

pub mod header;
pub mod log;
pub mod resume;

pub use header::{
    SESSION_FORMAT_VERSION, SessionEntry, SessionError, SessionHeader, SessionInfo,
    current_timestamp, generate_session_id,
};
pub use log::SessionManager;
pub use resume::{RestoredSession, replay_events, restore_session};

pub use crate::tool::todo::{TodoItem, TodoList};
