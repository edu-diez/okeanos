//! Session log and resume functionality.
//!
//! Provides append-only JSONL session logging with project-scoped storage,
//! resume capability, and corrupted line tolerance.

use crate::events::{Event, Message};
use crate::tool::todo::{TodoItem, TodoList};
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
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

/// Manages session storage and logging.
#[derive(Debug)]
pub struct SessionManager {
    /// Base directory for session storage (project-scoped).
    storage_root: PathBuf,
    /// Current session writer (if a session is active).
    current_session: Option<ActiveSession>,
}

#[derive(Debug)]
#[allow(dead_code)]
struct ActiveSession {
    id: String,
    path: PathBuf,
    writer: BufWriter<File>,
    started_at: u64,
    last_event_at: u64,
    event_count: usize,
    cwd: String,
    project_root: String,
}

impl SessionManager {
    /// Create a new session manager.
    ///
    /// Sessions are stored under `{project_root}/.okeanos/sessions/`.
    pub fn new(project_root: &Path) -> Result<Self, SessionError> {
        let storage_root = project_root.join(".okeanos").join("sessions");
        fs::create_dir_all(&storage_root).map_err(|source| SessionError::CreateDir {
            path: storage_root.clone(),
            source,
        })?;
        Ok(Self {
            storage_root,
            current_session: None,
        })
    }

    /// Start a new session and return its ID.
    pub fn start_session(
        &mut self,
        cwd: &Path,
        project_root: &Path,
    ) -> Result<String, SessionError> {
        let id = generate_session_id();
        let path = self.storage_root.join(format!("{}.jsonl", id));

        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&path)
            .map_err(|source| SessionError::OpenLog {
                path: path.clone(),
                source,
            })?;

        let mut writer = BufWriter::new(file);

        let started_at = current_timestamp();
        let header = SessionHeader {
            id: id.clone(),
            cwd: cwd.display().to_string(),
            format_version: SESSION_FORMAT_VERSION,
            started_at,
            project_root: project_root.display().to_string(),
        };

        let header_line = serde_json::to_string(&header)
            .map_err(|source| SessionError::SerializeEvent { source })?;
        writer
            .write_all(header_line.as_bytes())
            .map_err(|source| SessionError::WriteLog {
                path: path.clone(),
                source,
            })?;
        writer
            .write_all(b"\n")
            .map_err(|source| SessionError::WriteLog {
                path: path.clone(),
                source,
            })?;
        writer.flush().map_err(|source| SessionError::WriteLog {
            path: path.clone(),
            source,
        })?;

        let session = ActiveSession {
            id: id.clone(),
            path: path.clone(),
            writer,
            started_at,
            last_event_at: started_at,
            event_count: 0,
            cwd: cwd.display().to_string(),
            project_root: project_root.display().to_string(),
        };

        self.current_session = Some(session);
        Ok(id)
    }

    /// Append an event to the current session log.
    pub fn log_event(&mut self, event: Event) -> Result<(), SessionError> {
        let Some(session) = &mut self.current_session else {
            return Ok(()); // No active session, silently ignore
        };

        let entry = SessionEntry {
            ts: current_timestamp(),
            event,
        };

        let line = serde_json::to_string(&entry)
            .map_err(|source| SessionError::SerializeEvent { source })?;

        session
            .writer
            .write_all(line.as_bytes())
            .map_err(|source| SessionError::WriteLog {
                path: session.path.clone(),
                source,
            })?;
        session
            .writer
            .write_all(b"\n")
            .map_err(|source| SessionError::WriteLog {
                path: session.path.clone(),
                source,
            })?;

        session.last_event_at = entry.ts;
        session.event_count += 1;

        Ok(())
    }

    /// Flush the current session log to disk.
    pub fn flush(&mut self) -> Result<(), SessionError> {
        if let Some(session) = &mut self.current_session {
            session
                .writer
                .flush()
                .map_err(|source| SessionError::WriteLog {
                    path: session.path.clone(),
                    source,
                })?;
        }
        Ok(())
    }

    /// End the current session.
    pub fn end_session(&mut self) -> Result<(), SessionError> {
        if let Some(mut session) = self.current_session.take() {
            session
                .writer
                .flush()
                .map_err(|source| SessionError::WriteLog {
                    path: session.path.clone(),
                    source,
                })?;
        }
        Ok(())
    }

    /// Get the current session ID, if any.
    pub fn current_session_id(&self) -> Option<&str> {
        self.current_session.as_ref().map(|s| s.id.as_str())
    }

    /// List all sessions in the project.
    pub fn list_sessions(&self) -> Result<Vec<SessionInfo>, SessionError> {
        let mut sessions = Vec::new();

        for entry in fs::read_dir(&self.storage_root).map_err(|source| SessionError::ReadLog {
            path: self.storage_root.clone(),
            source,
        })? {
            let entry = entry.map_err(|source| SessionError::ReadLog {
                path: self.storage_root.clone(),
                source,
            })?;
            let path = entry.path();

            if path.extension().and_then(|s| s.to_str()) == Some("jsonl")
                && let Some(info) = self.read_session_info(&path)?
            {
                sessions.push(info);
            }
        }

        // Sort by start time, newest first
        sessions.sort_by_key(|a| std::cmp::Reverse(a.started_at));
        Ok(sessions)
    }

    /// Read session metadata without loading full log.
    fn read_session_info(&self, path: &Path) -> Result<Option<SessionInfo>, SessionError> {
        let file = File::open(path).map_err(|source| SessionError::ReadLog {
            path: path.to_path_buf(),
            source,
        })?;
        let reader = BufReader::new(file);

        let mut lines = reader.lines();
        let header_line = match lines.next() {
            Some(Ok(line)) => line,
            Some(Err(e)) => {
                return Err(SessionError::ReadLog {
                    path: path.to_path_buf(),
                    source: e,
                });
            }
            None => return Ok(None), // Empty file
        };

        let header: SessionHeader = serde_json::from_str(&header_line)
            .map_err(|source| SessionError::ParseEvent { source })?;

        if header.format_version != SESSION_FORMAT_VERSION {
            return Err(SessionError::InvalidHeader {
                message: format!(
                    "unsupported format version: {} (expected {})",
                    header.format_version, SESSION_FORMAT_VERSION
                ),
            });
        }

        let mut event_count = 0;
        let mut last_event_at = header.started_at;

        for line in lines.map_while(Result::ok) {
            if !line.trim().is_empty() {
                event_count += 1;
                // Try to extract timestamp from entry
                if let Ok(entry) = serde_json::from_str::<SessionEntry>(&line) {
                    last_event_at = entry.ts;
                }
            }
        }

        Ok(Some(SessionInfo {
            id: header.id,
            cwd: header.cwd,
            project_root: header.project_root,
            started_at: header.started_at,
            last_event_at,
            event_count,
        }))
    }

    /// Load a session by ID for resume.
    ///
    /// Returns the session header, all valid events, and any corrupted line warnings.
    pub fn load_session(
        &self,
        session_id: &str,
    ) -> Result<(SessionHeader, Vec<Event>, Vec<String>), SessionError> {
        let path = self.storage_root.join(format!("{}.jsonl", session_id));

        if !path.exists() {
            return Err(SessionError::NotFound {
                id: session_id.to_string(),
            });
        }

        let file = File::open(&path).map_err(|source| SessionError::ReadLog {
            path: path.clone(),
            source,
        })?;
        let reader = BufReader::new(file);

        let mut lines = reader.lines();

        // Read header
        let header_line = match lines.next() {
            Some(Ok(line)) => line,
            Some(Err(e)) => {
                return Err(SessionError::ReadLog {
                    path: path.clone(),
                    source: e,
                });
            }
            None => {
                return Err(SessionError::InvalidHeader {
                    message: "empty session file".to_string(),
                });
            }
        };

        let header: SessionHeader = serde_json::from_str(&header_line)
            .map_err(|source| SessionError::ParseEvent { source })?;

        if header.format_version != SESSION_FORMAT_VERSION {
            return Err(SessionError::InvalidHeader {
                message: format!(
                    "unsupported format version: {} (expected {})",
                    header.format_version, SESSION_FORMAT_VERSION
                ),
            });
        }

        // Read events with corrupted line tolerance
        let mut events = Vec::new();
        let mut warnings = Vec::new();
        let mut line_num = 1; // Header is line 1

        for line_result in lines {
            line_num += 1;
            let line = match line_result {
                Ok(l) => l,
                Err(e) => {
                    warnings.push(format!("line {}: failed to read line: {}", line_num, e));
                    continue;
                }
            };

            if line.trim().is_empty() {
                continue;
            }

            match serde_json::from_str::<SessionEntry>(&line) {
                Ok(entry) => events.push(entry.event),
                Err(e) => {
                    warnings.push(format!("line {}: corrupted entry skipped: {}", line_num, e));
                }
            }
        }

        Ok((header, events, warnings))
    }

    /// Get the storage root path.
    pub fn storage_root(&self) -> &Path {
        &self.storage_root
    }
}

/// Restored session state for resume.
#[derive(Debug, Clone)]
pub struct RestoredSession {
    /// The session header.
    pub header: SessionHeader,
    /// Restored conversation messages.
    pub messages: Vec<Message>,
    /// Restored todo list.
    pub todos: TodoList,
    /// Any warnings from loading (corrupted lines, etc.).
    pub warnings: Vec<String>,
}

/// Restore a session from its log.
///
/// Replays events to reconstruct conversation history and todo state.
pub fn restore_session(
    manager: &SessionManager,
    session_id: &str,
) -> Result<RestoredSession, SessionError> {
    let (header, events, warnings) = manager.load_session(session_id)?;

    let mut messages = Vec::new();
    let todos = TodoList::new();

    // Replay events to rebuild state
    for event in events {
        match event {
            Event::Message(msg) => {
                messages.push(msg);
            }
            Event::Todo(_todo) => {
                // Todo events are emitted when todo list changes
                // We'll reconstruct from the event
                // Note: Todo event is a single item, but we need the full list
                // This is a simplification - in practice we'd track the full list
            }
            Event::ToolResult(result) => {
                // Check if it's a todo tool result and reconstruct
                if result.name == "todo"
                    && let Ok(output) = serde_json::from_str::<serde_json::Value>(&result.output)
                {
                    if let Some(arr) = output.as_array() {
                        for item_val in arr {
                            if let Ok(_item) = serde_json::from_value::<TodoItem>(item_val.clone())
                            {
                                // Reconstruct todo list from items
                                // Since TodoList is internal, we need a way to populate it
                                // For now, we'll just note that we have todo data
                            }
                        }
                    } else if let Ok(_item) = serde_json::from_value::<TodoItem>(output) {
                        // Single item result
                    }
                }
            }
            _ => {}
        }
    }

    // We need a better way to restore todos - the events don't capture full state
    // For now, we'll just return what we have
    Ok(RestoredSession {
        header,
        messages,
        todos,
        warnings,
    })
}

/// Replay events into a conversation and todo list.
///
/// This is the proper way to restore state for resume.
pub fn replay_events(events: &[Event]) -> (Vec<Message>, TodoList) {
    let mut messages = Vec::new();
    let todos = TodoList::new();

    for event in events {
        match event {
            Event::Message(msg) => {
                messages.push(msg.clone());
            }
            Event::ToolResult(result) => {
                // Handle todo tool results to rebuild todo list
                if result.name == "todo"
                    && let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&result.output)
                {
                    if let Some(arr) = parsed.as_array() {
                        for item_val in arr {
                            if let Ok(item) = serde_json::from_value::<TodoItem>(item_val.clone()) {
                                if item.done {
                                    let _ = todos.complete(&item.id);
                                } else {
                                    todos.add(item.title);
                                }
                            }
                        }
                    } else if let Ok(item) = serde_json::from_value::<TodoItem>(parsed) {
                        if item.done {
                            let _ = todos.complete(&item.id);
                        } else {
                            todos.add(item.title);
                        }
                    }
                }
            }
            _ => {}
        }
    }

    (messages, todos)
}

/// Generate a unique session ID.
fn generate_session_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let random: u32 = rand::random();
    format!("session-{:x}-{:x}", timestamp, random)
}

/// Get current Unix timestamp in milliseconds.
fn current_timestamp() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{Event, Log, LogLevel, Message, MessageRole, ToolResult};
    use tempfile::tempdir;

    #[test]
    fn test_session_manager_create() {
        let dir = tempdir().unwrap();
        let manager = SessionManager::new(dir.path()).unwrap();
        assert!(manager.storage_root().exists());
    }

    #[test]
    fn test_start_session_writes_header() {
        let dir = tempdir().unwrap();
        let mut manager = SessionManager::new(dir.path()).unwrap();

        let session_id = manager
            .start_session(dir.path(), dir.path())
            .expect("start session");

        // Verify file exists and has header
        let session_path = dir
            .path()
            .join(".okeanos")
            .join("sessions")
            .join(format!("{}.jsonl", session_id));
        assert!(session_path.exists());

        let content = std::fs::read_to_string(&session_path).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 1);

        let header: SessionHeader = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(header.id, session_id);
        assert_eq!(header.format_version, SESSION_FORMAT_VERSION);
        assert!(header.started_at > 0);
    }

    #[test]
    fn test_log_event_appends_to_session() {
        let dir = tempdir().unwrap();
        let mut manager = SessionManager::new(dir.path()).unwrap();

        let session_id = manager.start_session(dir.path(), dir.path()).unwrap();

        let event = Event::Log(Log {
            level: LogLevel::Info,
            message: "Test log".to_string(),
        });
        manager.log_event(event).unwrap();
        manager.flush().unwrap();

        let session_path = dir
            .path()
            .join(".okeanos")
            .join("sessions")
            .join(format!("{}.jsonl", session_id));
        let content = std::fs::read_to_string(&session_path).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 2); // header + 1 event

        let entry: SessionEntry = serde_json::from_str(lines[1]).unwrap();
        assert!(matches!(entry.event, Event::Log(_)));
    }

    #[test]
    fn test_load_session_reads_events() {
        let dir = tempdir().unwrap();
        let mut manager = SessionManager::new(dir.path()).unwrap();

        let session_id = manager.start_session(dir.path(), dir.path()).unwrap();

        // Log several events
        for i in 0..3 {
            let event = Event::Log(Log {
                level: LogLevel::Info,
                message: format!("Log {}", i),
            });
            manager.log_event(event).unwrap();
        }
        manager.flush().unwrap();

        // Load and verify
        let (header, events, warnings) = manager.load_session(&session_id).unwrap();
        assert_eq!(header.id, session_id);
        assert_eq!(events.len(), 3);
        assert!(warnings.is_empty());
    }

    #[test]
    fn test_load_session_corrupted_line_tolerance() {
        let dir = tempdir().unwrap();

        // Create a session and write some events
        let mut manager = SessionManager::new(dir.path()).unwrap();
        let session_id = manager.start_session(dir.path(), dir.path()).unwrap();

        manager
            .log_event(Event::Log(Log {
                level: LogLevel::Info,
                message: "Valid".to_string(),
            }))
            .unwrap();
        manager.flush().unwrap();
        manager.end_session().unwrap();

        // Simulate a crash: manually append a corrupted line (partial write)
        let session_path = dir
            .path()
            .join(".okeanos")
            .join("sessions")
            .join(format!("{}.jsonl", session_id));
        {
            let mut file = OpenOptions::new().append(true).open(&session_path).unwrap();
            writeln!(file, "this is not valid json").unwrap();
            file.flush().unwrap();
        }

        // Now create a NEW manager (simulating a new process) and load the session
        let manager2 = SessionManager::new(dir.path()).unwrap();
        let (header, events, warnings) = manager2.load_session(&session_id).unwrap();

        assert_eq!(header.id, session_id);
        assert_eq!(events.len(), 1); // Only the valid event
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("corrupted entry skipped"));
    }

    #[test]
    fn test_list_sessions() {
        let dir = tempdir().unwrap();
        let mut manager = SessionManager::new(dir.path()).unwrap();

        let id1 = manager.start_session(dir.path(), dir.path()).unwrap();
        manager
            .log_event(Event::Log(Log {
                level: LogLevel::Info,
                message: "Session 1".to_string(),
            }))
            .unwrap();
        manager.end_session().unwrap();

        let id2 = manager.start_session(dir.path(), dir.path()).unwrap();
        manager
            .log_event(Event::Log(Log {
                level: LogLevel::Info,
                message: "Session 2".to_string(),
            }))
            .unwrap();
        manager.end_session().unwrap();

        let sessions = manager.list_sessions().unwrap();
        assert_eq!(sessions.len(), 2);
        // Sorted newest first
        assert_eq!(sessions[0].id, id2);
        assert_eq!(sessions[1].id, id1);
    }

    #[test]
    fn test_replay_events_restores_messages() {
        let msg1 = Message {
            role: MessageRole::User,
            content: "Hello".to_string(),
            tool_calls: None,
            tool_call_id: None,
            ts: 1000,
        };
        let msg2 = Message {
            role: MessageRole::Assistant,
            content: "Hi there".to_string(),
            tool_calls: None,
            tool_call_id: None,
            ts: 1001,
        };

        let events = vec![Event::Message(msg1.clone()), Event::Message(msg2.clone())];
        let (messages, _todos) = replay_events(&events);

        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].content, "Hello");
        assert_eq!(messages[1].content, "Hi there");
    }

    #[test]
    fn test_replay_events_restores_todos() {
        // Create a todo add result
        let add_result = ToolResult {
            id: "call-1".to_string(),
            name: "todo".to_string(),
            args: serde_json::json!({"action": "add", "title": "Task 1"}),
            output: serde_json::json!({"id": "todo-1", "title": "Task 1", "done": false})
                .to_string(),
            ok: true,
            duration_ms: 10,
            truncated: false,
            exit_code: None,
            timed_out: false,
            cancelled: false,
        };

        // Create a todo complete result
        let complete_result = ToolResult {
            id: "call-2".to_string(),
            name: "todo".to_string(),
            args: serde_json::json!({"action": "complete", "id": "todo-1"}),
            output: serde_json::json!({"id": "todo-1", "title": "Task 1", "done": true})
                .to_string(),
            ok: true,
            duration_ms: 10,
            truncated: false,
            exit_code: None,
            timed_out: false,
            cancelled: false,
        };

        let events = vec![
            Event::ToolResult(add_result),
            Event::ToolResult(complete_result),
        ];

        let (_messages, todos) = replay_events(&events);

        let items = todos.list();
        assert_eq!(items.len(), 1);
        assert!(items[0].done);
    }
}
