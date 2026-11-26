//! Append-only JSONL session logging with rotation and compression.

use crate::config::RotationSection;
use crate::events::Event;
use crate::session::header::{
    SESSION_FORMAT_VERSION, SessionEntry, SessionError, SessionHeader, SessionInfo,
    current_timestamp, generate_session_id,
};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

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
    /// Current file size in bytes (tracked for size-based rotation).
    current_size: u64,
    /// Segment index (0 = active, incremented on rotation).
    segment_index: u32,
}

/// Compression worker state.
#[derive(Debug)]
struct CompressionWorker {
    storage_root: PathBuf,
    config: RotationSection,
    shutdown_tx: Option<tokio::sync::oneshot::Sender<()>>,
    handle: Option<tokio::task::JoinHandle<()>>,
}

impl CompressionWorker {
    fn new(storage_root: PathBuf, config: RotationSection) -> Self {
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
        let storage_root_clone = storage_root.clone();
        let config_clone = config.clone();

        let handle = tokio::spawn(async move {
            Self::run_worker(storage_root_clone, config_clone, shutdown_rx).await;
        });

        Self {
            storage_root,
            config,
            shutdown_tx: Some(shutdown_tx),
            handle: Some(handle),
        }
    }

    async fn run_worker(
        storage_root: PathBuf,
        config: RotationSection,
        mut shutdown_rx: tokio::sync::oneshot::Receiver<()>,
    ) {
        let interval_sec = config.background_worker_interval_sec.unwrap_or(1800);
        let mut interval = tokio::time::interval(Duration::from_secs(interval_sec));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        // Initial run after a short delay
        tokio::time::sleep(Duration::from_secs(5)).await;
        Self::compress_eligible_segments(&storage_root, &config).await;

        loop {
            tokio::select! {
                _ = interval.tick() => {
                    Self::compress_eligible_segments(&storage_root, &config).await;
                }
                _ = &mut shutdown_rx => {
                    break;
                }
            }
        }
    }

    async fn compress_eligible_segments(storage_root: &Path, config: &RotationSection) {
        if !config.compress_after_rotation.unwrap_or(true) {
            return;
        }

        let max_age_days = config.max_age_days.unwrap_or(30);
        let min_age = Duration::from_secs(max_age_days * 24 * 60 * 60);
        let min_size = 1_048_576; // 1 MB minimum size to compress

        let entries = match fs::read_dir(storage_root) {
            Ok(e) => e,
            Err(_) => return,
        };

        for entry in entries.flatten() {
            let path = entry.path();
            let filename = match path.file_name().and_then(|s| s.to_str()) {
                Some(s) => s,
                None => continue,
            };

            // Skip active segment (no segment index suffix)
            if filename.ends_with(".jsonl") && !filename.contains(".jsonl.") {
                continue;
            }

            // Skip already compressed files
            if filename.ends_with(".zst") || filename.ends_with(".gz") {
                continue;
            }

            // Check if it's a rotated segment (session-id.N.jsonl)
            if !Self::is_rotated_segment(filename) {
                continue;
            }

            // Check file metadata
            let metadata = match fs::metadata(&path) {
                Ok(m) => m,
                Err(_) => continue,
            };

            let size = metadata.len();
            if size < min_size {
                continue;
            }

            let modified = match metadata.modified() {
                Ok(t) => t,
                Err(_) => continue,
            };

            if modified.elapsed().unwrap_or(Duration::MAX) < min_age {
                continue;
            }

            // Try to acquire exclusive lock to prevent concurrent access
            if let Ok(file) = File::open(&path) {
                use fd_lock::RwLock;
                let mut lock = RwLock::new(file);
                if lock.try_write().is_err() {
                    // File is locked by another process (active writer), skip
                    continue;
                }
                // We have the lock, compress now
                drop(lock);
                if let Err(e) = Self::compress_segment(&path, config).await {
                    eprintln!("Failed to compress segment {}: {}", path.display(), e);
                }
            }
        }

        // Cleanup old segments after compression
        Self::cleanup_old_segments(storage_root, config).await;
    }

    fn is_rotated_segment(filename: &str) -> bool {
        // Matches session-{id}.N.jsonl where N >= 1
        if let Some(stem) = filename.strip_suffix(".jsonl")
            && let Some(last_dot) = stem.rfind('.')
        {
            let suffix = &stem[last_dot + 1..];
            if let Ok(idx) = suffix.parse::<u32>() {
                return idx >= 1;
            }
        }
        false
    }

    async fn compress_segment(
        path: &Path,
        config: &RotationSection,
    ) -> Result<PathBuf, SessionError> {
        let data = fs::read(path).map_err(|source| SessionError::ReadLog {
            path: path.to_path_buf(),
            source,
        })?;

        let compression = config.compression.as_deref().unwrap_or("zstd");
        let compressed_data = match compression {
            "zstd" => {
                let level = config.zstd_level.unwrap_or(3);
                let mut encoder = zstd::stream::Encoder::new(Vec::new(), level).map_err(|e| {
                    SessionError::Compression {
                        message: format!("zstd encoder creation failed: {e}"),
                    }
                })?;
                if config.zstd_long_distance_matching.unwrap_or(true) {
                    encoder.long_distance_matching(true).map_err(|e| {
                        SessionError::Compression {
                            message: format!("zstd long distance matching failed: {e}"),
                        }
                    })?;
                    // Use a 32 MiB window (windowLog 25) for repeated content
                    encoder
                        .window_log(25)
                        .map_err(|e| SessionError::Compression {
                            message: format!("zstd window log failed: {e}"),
                        })?;
                }
                encoder
                    .write_all(&data)
                    .map_err(|e| SessionError::Compression {
                        message: format!("zstd write failed: {e}"),
                    })?;
                encoder.finish().map_err(|e| SessionError::Compression {
                    message: format!("zstd finish failed: {e}"),
                })?
            }
            "gzip" => {
                use flate2::Compression;
                use flate2::write::GzEncoder;
                let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
                encoder
                    .write_all(&data)
                    .map_err(|e| SessionError::Compression {
                        message: format!("gzip write failed: {e}"),
                    })?;
                encoder.finish().map_err(|e| SessionError::Compression {
                    message: format!("gzip finish failed: {e}"),
                })?
            }
            _ => {
                return Err(SessionError::Compression {
                    message: format!("unknown compression algorithm: {compression}"),
                });
            }
        };

        let compressed_path = if compression == "gzip" {
            path.with_extension("jsonl.gz")
        } else {
            path.with_extension("jsonl.zst")
        };

        fs::write(&compressed_path, compressed_data).map_err(|source| SessionError::WriteLog {
            path: compressed_path.clone(),
            source,
        })?;

        // Remove original after successful compression
        fs::remove_file(path).map_err(|source| SessionError::WriteLog {
            path: path.to_path_buf(),
            source,
        })?;

        Ok(compressed_path)
    }

    async fn cleanup_old_segments(storage_root: &Path, config: &RotationSection) {
        let keep_segments = config.keep_rotated_segments.unwrap_or(20);
        let max_age_days = config.max_age_days.unwrap_or(30);
        let max_age = Duration::from_secs(max_age_days * 24 * 60 * 60);

        // Group segments by session ID
        let mut segments_by_session: std::collections::HashMap<String, Vec<PathBuf>> =
            std::collections::HashMap::new();

        let entries = match fs::read_dir(storage_root) {
            Ok(e) => e,
            Err(_) => return,
        };

        for entry in entries.flatten() {
            let path = entry.path();
            let filename = match path.file_name().and_then(|s| s.to_str()) {
                Some(s) => s,
                None => continue,
            };

            // Skip active segment
            if filename.ends_with(".jsonl") && !filename.contains(".jsonl.") {
                continue;
            }

            // Extract session ID and segment index
            if let Some((session_id, segment_idx)) = Self::parse_segment_filename(filename)
                && segment_idx >= 1
            {
                segments_by_session
                    .entry(session_id)
                    .or_default()
                    .push(path);
            }
        }

        // Clean up each session's segments
        for (_session_id, mut paths) in segments_by_session {
            // Sort by segment index (newest first)
            paths.sort_by(|a, b| {
                let idx_a =
                    Self::parse_segment_filename(a.file_name().unwrap().to_string_lossy().as_ref())
                        .map(|(_, idx)| idx)
                        .unwrap_or(0);
                let idx_b =
                    Self::parse_segment_filename(b.file_name().unwrap().to_string_lossy().as_ref())
                        .map(|(_, idx)| idx)
                        .unwrap_or(0);
                idx_b.cmp(&idx_a)
            });

            // Keep only the most recent `keep_segments`
            if keep_segments > 0 && paths.len() > keep_segments {
                for path in paths.drain(keep_segments..) {
                    let _ = fs::remove_file(&path);
                }
            }

            // Also remove segments older than max_age_days
            for path in paths {
                if let Ok(metadata) = fs::metadata(&path)
                    && let Ok(modified) = metadata.modified()
                    && modified.elapsed().unwrap_or(Duration::ZERO) > max_age
                {
                    let _ = fs::remove_file(&path);
                }
            }
        }
    }

    fn parse_segment_filename(filename: &str) -> Option<(String, u32)> {
        // session-{id}.N.jsonl or session-{id}.N.jsonl.zst
        let base = filename.strip_suffix(".zst").unwrap_or(filename);
        let base = base.strip_suffix(".gz").unwrap_or(base);

        if let Some(stem) = base.strip_suffix(".jsonl")
            && let Some(last_dot) = stem.rfind('.')
        {
            let session_id = stem[..last_dot].to_string();
            let suffix = &stem[last_dot + 1..];
            if let Ok(idx) = suffix.parse::<u32>() {
                return Some((session_id, idx));
            }
        }
        None
    }
}

impl Drop for CompressionWorker {
    fn drop(&mut self) {
        // Use take() to move out the sender
        if let Some(shutdown_tx) = self.shutdown_tx.take() {
            let _ = shutdown_tx.send(());
        }
        if let Some(handle) = self.handle.take() {
            // Don't await in drop, just detach
            handle.abort();
        }
    }
}

/// Manages session storage and logging.
#[derive(Debug)]
pub struct SessionManager {
    /// Base directory for session storage (project-scoped).
    storage_root: PathBuf,
    /// Current session writer (if a session is active).
    current_session: Option<ActiveSession>,
    /// Rotation configuration.
    rotation_config: Option<RotationSection>,
    /// Compression worker (runs in background).
    compression_worker: Option<Arc<Mutex<CompressionWorker>>>,
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
            rotation_config: None,
            compression_worker: None,
        })
    }

    /// Create a new session manager with rotation configuration.
    pub fn new_with_config(
        project_root: &Path,
        rotation_config: Option<RotationSection>,
    ) -> Result<Self, SessionError> {
        let storage_root = project_root.join(".okeanos").join("sessions");
        fs::create_dir_all(&storage_root).map_err(|source| SessionError::CreateDir {
            path: storage_root.clone(),
            source,
        })?;

        let mut manager = Self {
            storage_root,
            current_session: None,
            rotation_config: rotation_config.clone(),
            compression_worker: None,
        };

        // Start compression worker if rotation is enabled
        if rotation_config
            .as_ref()
            .map(|c| c.enabled.unwrap_or(true))
            .unwrap_or(false)
        {
            let worker =
                CompressionWorker::new(manager.storage_root.clone(), rotation_config.unwrap());
            manager.compression_worker = Some(Arc::new(Mutex::new(worker)));
        }

        Ok(manager)
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
            segment_index: 0,
            parent_segment: None,
            is_compressed: false,
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

        let header_len = header_line.len() as u64 + 1; // +1 for newline

        let session = ActiveSession {
            id: id.clone(),
            path: path.clone(),
            writer,
            started_at,
            last_event_at: started_at,
            event_count: 0,
            cwd: cwd.display().to_string(),
            project_root: project_root.display().to_string(),
            current_size: header_len,
            segment_index: 0,
        };

        self.current_session = Some(session);
        Ok(id)
    }

    /// Append an event to the current session log.
    /// Returns true if rotation occurred.
    pub fn log_event(&mut self, event: Event) -> Result<bool, SessionError> {
        // Live tool output is ephemeral; `tool_result` carries the final output.
        if matches!(event, Event::ToolOutput(_)) {
            return Ok(false);
        }

        // Check if we should rotate on compaction boundary (before borrowing session)
        let should_rotate_compaction = matches!(&event, Event::Compaction(_))
            && self
                .rotation_config
                .as_ref()
                .map(|c| c.rotate_on_compaction.unwrap_or(true))
                .unwrap_or(false);

        // Check size-based rotation (need to borrow session for current_size)
        let should_rotate_size = {
            let Some(session) = &self.current_session else {
                return Ok(false); // No active session, silently ignore
            };
            let max_size_mb = self
                .rotation_config
                .as_ref()
                .and_then(|c| c.max_file_size_mb)
                .unwrap_or(50);
            let max_size_bytes = max_size_mb * 1_048_576;
            session.current_size >= max_size_bytes
        };

        if should_rotate_compaction || should_rotate_size {
            self.rotate_session()?;
        }

        let Some(session) = &mut self.current_session else {
            return Ok(false); // No active session after rotation (shouldn't happen)
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

        // Flush to ensure data is persisted (important for rotation and crash safety)
        session
            .writer
            .flush()
            .map_err(|source| SessionError::WriteLog {
                path: session.path.clone(),
                source,
            })?;

        let line_len = line.len() as u64 + 1; // +1 for newline
        session.current_size += line_len;
        session.last_event_at = entry.ts;
        session.event_count += 1;

        Ok(should_rotate_compaction || should_rotate_size)
    }

    /// Rotate the current session to a new segment file.
    fn rotate_session(&mut self) -> Result<(), SessionError> {
        let Some(mut session) = self.current_session.take() else {
            return Ok(());
        };

        // Flush current writer
        session
            .writer
            .flush()
            .map_err(|source| SessionError::WriteLog {
                path: session.path.clone(),
                source,
            })?;

        // Determine new segment index
        let new_segment_index = session.segment_index + 1;

        // Create new segment filename: session-{id}.N.jsonl
        let stem = session
            .path
            .file_stem()
            .unwrap()
            .to_string_lossy()
            .to_string();
        let new_filename = format!("{}.{}.jsonl", stem, new_segment_index);
        let new_path = session.path.with_file_name(new_filename);

        // Rename current file to segment file
        fs::rename(&session.path, &new_path).map_err(|source| SessionError::WriteLog {
            path: new_path.clone(),
            source,
        })?;

        // Create new active segment file
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&session.path)
            .map_err(|source| SessionError::OpenLog {
                path: session.path.clone(),
                source,
            })?;

        let mut writer = BufWriter::new(file);

        // Write header for new active segment
        let header = SessionHeader {
            id: session.id.clone(),
            cwd: session.cwd.clone(),
            format_version: SESSION_FORMAT_VERSION,
            started_at: session.started_at,
            project_root: session.project_root.clone(),
            segment_index: 0, // Active segment is always 0
            parent_segment: Some(new_path.file_name().unwrap().to_string_lossy().to_string()),
            is_compressed: false,
        };

        let header_line = serde_json::to_string(&header)
            .map_err(|source| SessionError::SerializeEvent { source })?;
        writer
            .write_all(header_line.as_bytes())
            .map_err(|source| SessionError::WriteLog {
                path: session.path.clone(),
                source,
            })?;
        writer
            .write_all(b"\n")
            .map_err(|source| SessionError::WriteLog {
                path: session.path.clone(),
                source,
            })?;
        writer.flush().map_err(|source| SessionError::WriteLog {
            path: session.path.clone(),
            source,
        })?;

        let header_len = header_line.len() as u64 + 1;

        // Update rotated segment's header to mark its segment index
        Self::update_segment_header(
            &new_path,
            new_segment_index,
            Some(
                session
                    .path
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .to_string(),
            ),
        )?;

        // Update session to new active segment
        session.path = session.path.clone(); // Same path (active segment)
        session.writer = writer;
        session.current_size = header_len;
        session.segment_index = 0;
        session.event_count = 0;

        self.current_session = Some(session);
        Ok(())
    }

    /// Update the header of a rotated segment file.
    fn update_segment_header(
        path: &Path,
        segment_index: u32,
        parent_segment: Option<String>,
    ) -> Result<(), SessionError> {
        let file = File::open(path).map_err(|source| SessionError::ReadLog {
            path: path.to_path_buf(),
            source,
        })?;
        let reader = BufReader::new(file);

        let mut lines: Vec<String> =
            reader
                .lines()
                .collect::<Result<_, _>>()
                .map_err(|source| SessionError::ReadLog {
                    path: path.to_path_buf(),
                    source,
                })?;

        if lines.is_empty() {
            return Err(SessionError::InvalidHeader {
                message: "empty segment file".to_string(),
            });
        }

        // Update header
        let mut header: SessionHeader = serde_json::from_str(&lines[0])
            .map_err(|source| SessionError::ParseEvent { source })?;

        header.segment_index = segment_index;
        header.parent_segment = parent_segment;

        lines[0] = serde_json::to_string(&header)
            .map_err(|source| SessionError::SerializeEvent { source })?;

        // Write back
        let content = lines.join("\n") + "\n";
        fs::write(path, content).map_err(|source| SessionError::WriteLog {
            path: path.to_path_buf(),
            source,
        })?;

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

            // Only list active segments (segment_index == 0)
            if path.extension().and_then(|s| s.to_str()) == Some("jsonl")
                && !Self::is_rotated_segment_name(
                    path.file_name().and_then(|s| s.to_str()).unwrap_or(""),
                )
                && let Some(info) = self.read_session_info(&path)?
            {
                sessions.push(info);
            }
        }

        // Sort by start time, newest first
        sessions.sort_by_key(|a| std::cmp::Reverse(a.started_at));
        Ok(sessions)
    }

    fn is_rotated_segment_name(filename: &str) -> bool {
        if let Some(stem) = filename.strip_suffix(".jsonl")
            && let Some(last_dot) = stem.rfind('.')
        {
            let suffix = &stem[last_dot + 1..];
            if let Ok(idx) = suffix.parse::<u32>() {
                return idx >= 1;
            }
        }
        false
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

        // Only include active segments (segment_index == 0)
        if header.segment_index != 0 {
            return Ok(None);
        }

        let mut event_count = 0;
        let mut last_event_at = header.started_at;

        for line in lines.map_while(Result::ok) {
            if !line.trim().is_empty() {
                event_count += 1;
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

    /// Load a session by ID for resume, including all rotated segments.
    ///
    /// Returns the session header, all valid events (oldest first), and any corrupted line warnings.
    pub fn load_session(
        &self,
        session_id: &str,
    ) -> Result<(SessionHeader, Vec<Event>, Vec<String>), SessionError> {
        // Find all segments for this session
        let segments = self.find_session_segments(session_id)?;

        if segments.is_empty() {
            return Err(SessionError::NotFound {
                id: session_id.to_string(),
            });
        }

        // Load events from all segments in order (oldest first)
        let mut all_events = Vec::new();
        let mut all_warnings = Vec::new();
        let mut header = None;

        for (segment_index, segment_path) in segments {
            let (seg_header, events, warnings) = self.load_segment(&segment_path, segment_index)?;
            if header.is_none() {
                header = Some(seg_header);
            }
            all_events.extend(events);
            all_warnings.extend(warnings);
        }

        Ok((header.unwrap(), all_events, all_warnings))
    }

    /// Find all segments for a session, sorted by segment index (oldest first).
    fn find_session_segments(&self, session_id: &str) -> Result<Vec<(u32, PathBuf)>, SessionError> {
        let mut segments = Vec::new();
        let prefix = format!("{session_id}.");

        for entry in fs::read_dir(&self.storage_root).map_err(|source| SessionError::ReadLog {
            path: self.storage_root.clone(),
            source,
        })? {
            let entry = entry.map_err(|source| SessionError::ReadLog {
                path: self.storage_root.clone(),
                source,
            })?;
            let path = entry.path();
            let filename = match path.file_name().and_then(|s| s.to_str()) {
                Some(s) => s,
                None => continue,
            };

            // Check for active segment
            if filename == format!("{session_id}.jsonl") {
                segments.push((0, path));
                continue;
            }

            // Check for rotated segments: session-id.N.jsonl or .jsonl.zst/.gz
            if filename.starts_with(&prefix)
                && let Some((_, idx)) = Self::parse_segment_filename(filename)
            {
                segments.push((idx, path));
            }
        }

        // Sort by segment index (oldest first)
        segments.sort_by_key(|(idx, _)| *idx);
        Ok(segments)
    }

    fn parse_segment_filename(filename: &str) -> Option<(String, u32)> {
        // session-{id}.N.jsonl or session-{id}.N.jsonl.zst
        let base = filename.strip_suffix(".zst").unwrap_or(filename);
        let base = base.strip_suffix(".gz").unwrap_or(base);

        if let Some(stem) = base.strip_suffix(".jsonl")
            && let Some(last_dot) = stem.rfind('.')
        {
            let session_id = stem[..last_dot].to_string();
            let suffix = &stem[last_dot + 1..];
            if let Ok(idx) = suffix.parse::<u32>() {
                return Some((session_id, idx));
            }
        }
        None
    }

    /// Load a single segment (handles both plain and compressed).
    fn load_segment(
        &self,
        path: &Path,
        segment_index: u32,
    ) -> Result<(SessionHeader, Vec<Event>, Vec<String>), SessionError> {
        let is_compressed = path.extension().and_then(|s| s.to_str()) == Some("zst")
            || path.extension().and_then(|s| s.to_str()) == Some("gz");

        let data =
            if is_compressed {
                let compressed_data = fs::read(path).map_err(|source| SessionError::ReadLog {
                    path: path.to_path_buf(),
                    source,
                })?;
                let extension = path.extension().and_then(|s| s.to_str()).unwrap_or("");
                match extension {
                    "zst" => {
                        let mut decoder = zstd::stream::Decoder::new(&compressed_data[..])
                            .map_err(|e| SessionError::Decompression {
                                message: format!("zstd decoder creation failed: {e}"),
                            })?;
                        let mut decompressed = Vec::new();
                        std::io::copy(&mut decoder, &mut decompressed).map_err(|e| {
                            SessionError::Decompression {
                                message: format!("zstd decompression failed: {e}"),
                            }
                        })?;
                        decompressed
                    }
                    "gz" => {
                        use flate2::read::GzDecoder;
                        let mut decoder = GzDecoder::new(&compressed_data[..]);
                        let mut decompressed = Vec::new();
                        std::io::copy(&mut decoder, &mut decompressed).map_err(|e| {
                            SessionError::Decompression {
                                message: format!("gzip decompression failed: {e}"),
                            }
                        })?;
                        decompressed
                    }
                    _ => {
                        return Err(SessionError::Decompression {
                            message: format!("unknown compression format: {extension}"),
                        });
                    }
                }
            } else {
                fs::read(path).map_err(|source| SessionError::ReadLog {
                    path: path.to_path_buf(),
                    source,
                })?
            };

        let content = String::from_utf8(data).map_err(|e| SessionError::ParseEvent {
            source: serde_json::Error::io(std::io::Error::new(std::io::ErrorKind::InvalidData, e)),
        })?;

        let mut lines = content.lines();
        let header_line = match lines.next() {
            Some(line) => line,
            None => {
                return Err(SessionError::InvalidHeader {
                    message: "empty segment file".to_string(),
                });
            }
        };

        let header: SessionHeader = serde_json::from_str(header_line)
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

        for line in lines {
            line_num += 1;
            if line.trim().is_empty() {
                continue;
            }

            match serde_json::from_str::<SessionEntry>(line) {
                Ok(entry) => events.push(entry.event),
                Err(e) => {
                    warnings.push(format!(
                        "segment {} line {}: corrupted entry skipped: {}",
                        segment_index, line_num, e
                    ));
                }
            }
        }

        Ok((header, events, warnings))
    }

    /// Get the storage root path.
    pub fn storage_root(&self) -> &Path {
        &self.storage_root
    }

    /// Manually trigger compression of eligible segments.
    pub async fn trigger_compression(&self) -> Result<(), SessionError> {
        if let Some(worker) = &self.compression_worker {
            let worker = worker.lock().await;
            let storage_root = worker.storage_root.clone();
            let config = worker.config.clone();
            drop(worker);
            CompressionWorker::compress_eligible_segments(&storage_root, &config).await;
            CompressionWorker::cleanup_old_segments(&storage_root, &config).await;
        }
        Ok(())
    }

    /// Manually rotate the current session to a new segment.
    pub fn rotate(&mut self) -> Result<(), SessionError> {
        self.rotate_session()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RotationSection;
    use crate::events::{Event, Message, MessageRole};
    use tempfile::tempdir;

    fn create_test_config() -> RotationSection {
        RotationSection {
            enabled: Some(true),
            rotate_on_compaction: Some(true),
            max_file_size_mb: Some(1), // 1 MB for testing
            compress_after_rotation: Some(true),
            compression: Some("zstd".to_string()),
            zstd_level: Some(3),
            zstd_long_distance_matching: Some(true),
            max_age_days: Some(30),
            keep_rotated_segments: Some(5),
            background_worker_interval_sec: Some(1800),
        }
    }

    #[tokio::test]
    async fn test_session_rotation_on_compaction() {
        let dir = tempdir().unwrap();
        let project_root = dir.path().to_path_buf();
        let cwd = project_root.clone();

        let mut manager =
            SessionManager::new_with_config(&project_root, Some(create_test_config())).unwrap();

        let session_id = manager.start_session(&cwd, &project_root).unwrap();

        // Log some events
        for i in 0..5 {
            let msg = Message {
                role: MessageRole::User,
                content: format!("Test message {}", i),
                tool_calls: None,
                tool_call_id: None,
                ts: 0,
                thinking: None,
                extras: Default::default(),
            };
            manager.log_event(Event::Message(msg)).unwrap();
        }

        // Trigger compaction (should cause rotation)
        let compaction_event = Event::Compaction(crate::events::CompactionEvent {
            trigger: crate::events::CompactionTrigger::Manual,
            strategy: crate::events::CompactionStrategy::Basic,
            tokens_before: 1000,
            tokens_after: 500,
            messages_removed: 2,
            turns_summarized: 1,
            native_provider: None,
            summary: "Test compaction".to_string(),
            ts: 0,
        });
        manager.log_event(compaction_event).unwrap();

        // Verify rotation occurred - there should be a rotated segment
        let segments = manager.find_session_segments(&session_id).unwrap();
        assert!(
            segments.len() >= 2,
            "Expected at least 2 segments after rotation, got {}",
            segments.len()
        );

        // Load session should work across segments
        let (header, events, warnings) = manager.load_session(&session_id).unwrap();
        assert_eq!(header.id, session_id);
        assert!(events.len() >= 6); // 5 messages + 1 compaction event
        assert!(warnings.is_empty());
    }

    #[tokio::test]
    async fn test_session_manual_rotation() {
        let dir = tempdir().unwrap();
        let project_root = dir.path().to_path_buf();
        let cwd = project_root.clone();

        let mut manager =
            SessionManager::new_with_config(&project_root, Some(create_test_config())).unwrap();

        let session_id = manager.start_session(&cwd, &project_root).unwrap();

        // Log some events
        for i in 0..3 {
            let msg = Message {
                role: MessageRole::User,
                content: format!("Test message {}", i),
                tool_calls: None,
                tool_call_id: None,
                ts: 0,
                thinking: None,
                extras: Default::default(),
            };
            manager.log_event(Event::Message(msg)).unwrap();
        }

        // Manual rotation
        manager.rotate().unwrap();

        // Verify rotation occurred
        let segments = manager.find_session_segments(&session_id).unwrap();
        assert!(
            segments.len() >= 2,
            "Expected at least 2 segments after manual rotation"
        );

        // Load session should work across segments
        let (header, events, warnings) = manager.load_session(&session_id).unwrap();
        assert_eq!(header.id, session_id);
        assert!(events.len() >= 3);
        assert!(warnings.is_empty());
    }

    #[tokio::test]
    async fn test_session_load_across_compressed_segments() {
        let dir = tempdir().unwrap();
        let project_root = dir.path().to_path_buf();
        let cwd = project_root.clone();

        let mut config = create_test_config();
        config.compress_after_rotation = Some(true);

        let mut manager = SessionManager::new_with_config(&project_root, Some(config)).unwrap();

        let session_id = manager.start_session(&cwd, &project_root).unwrap();

        // Log some events
        for i in 0..3 {
            let msg = Message {
                role: MessageRole::User,
                content: format!("Test message {}", i),
                tool_calls: None,
                tool_call_id: None,
                ts: 0,
                thinking: None,
                extras: Default::default(),
            };
            manager.log_event(Event::Message(msg)).unwrap();
        }

        // Manual rotation
        manager.rotate().unwrap();

        // Trigger compression manually
        manager.trigger_compression().await.unwrap();

        // Load session should work across compressed segments
        let (header, events, warnings) = manager.load_session(&session_id).unwrap();
        assert_eq!(header.id, session_id);
        assert!(events.len() >= 3);
        assert!(warnings.is_empty());
    }
}
