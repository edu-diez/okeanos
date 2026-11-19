//! Append-only JSONL session logging.

use crate::events::Event;
use crate::session::header::{
    SESSION_FORMAT_VERSION, SessionEntry, SessionError, SessionHeader, SessionInfo,
    current_timestamp, generate_session_id,
};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};

/// Active session data.
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

/// Manages session storage and logging.
#[derive(Debug)]
pub struct SessionManager {
    /// Base directory for session storage (project-scoped).
    storage_root: PathBuf,
    /// Current session writer (if a session is active).
    current_session: Option<ActiveSession>,
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
