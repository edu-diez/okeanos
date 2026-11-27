//! Background jobs: `run` with `background: true` plus the `job_output` / `job_kill` tools.
//!
//! A job is a `run`-style process (same guard, sandbox, env scrubbing) owned by a supervisor task
//! that appends merged stdout/stderr to a bounded tail buffer. `job_output` returns only the
//! output produced since the previous poll; `job_kill` stops the whole process tree. Jobs live
//! for the lifetime of the process: dropping the runtime drops the supervisors, whose tree
//! guards kill any job still running, so nothing outlives the session.

use crate::tool::builtin::run::{CAPTURE_PART, DRAIN_GRACE_MS, TreeGuard, spawn_piped, terminate};
use crate::tool::builtin::{MAX_OUTPUT_BYTES, truncate_output_with_spill};
use crate::tool::tool_trait::{Tool, ToolCategory, ToolError, ToolExecutionResult, ToolSpec};
use async_trait::async_trait;
use serde_json::Value;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::Duration;
use tokio::time::{Instant, timeout_at};

/// Running jobs allowed at once.
pub const MAX_RUNNING_JOBS: usize = 8;
/// Finished jobs kept for polling before the oldest are forgotten.
const MAX_FINISHED_JOBS: usize = 16;
const MAX_WAIT_SECS: u64 = 60;
const KILL_WAIT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JobStatus {
    Running,
    Exited(Option<i32>),
    Killed,
    TimedOut,
}

impl JobStatus {
    fn is_running(self) -> bool {
        self == JobStatus::Running
    }

    fn describe(self) -> String {
        match self {
            JobStatus::Running => "running".into(),
            JobStatus::Exited(Some(c)) => format!("exited with code {c}"),
            JobStatus::Exited(None) => "exited (terminated by signal)".into(),
            JobStatus::Killed => "killed".into(),
            JobStatus::TimedOut => "timed out and killed".into(),
        }
    }
}

#[derive(Debug)]
struct JobState {
    status: JobStatus,
    /// Most recent output, at most `CAPTURE_PART` bytes.
    tail: VecDeque<u8>,
    /// Bytes ever produced.
    total: u64,
    /// Bytes already handed to the model.
    cursor: u64,
}

#[derive(Debug)]
struct Job {
    id: u64,
    command: String,
    started: std::time::Instant,
    state: Mutex<JobState>,
    kill: tokio::sync::Notify,
}

impl Job {
    fn state(&self) -> MutexGuard<'_, JobState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn push(&self, chunk: &[u8]) {
        let mut s = self.state();
        s.tail.extend(chunk);
        s.total += chunk.len() as u64;
        if s.tail.len() > CAPTURE_PART {
            let excess = s.tail.len() - CAPTURE_PART;
            s.tail.drain(..excess);
        }
    }

    fn finish(&self, status: JobStatus) {
        self.state().status = status;
    }

    /// Output since the last poll (advancing the cursor) and the current status.
    fn take_output(&self) -> (String, bool, JobStatus) {
        let mut s = self.state();
        let first_kept = s.total - s.tail.len() as u64;
        let skipped = first_kept.saturating_sub(s.cursor);
        let start = s.cursor.max(first_kept) - first_kept;
        let bytes: Vec<u8> = s.tail.iter().skip(start as usize).copied().collect();
        s.cursor = s.total;
        let mut text = String::new();
        if skipped > 0 {
            text.push_str(&format!(
                "...[{skipped} bytes of earlier output dropped]...\n"
            ));
        }
        text.push_str(&String::from_utf8_lossy(&bytes));
        let (text, truncated) = truncate_output_with_spill(&text, MAX_OUTPUT_BYTES);
        (text, truncated, s.status)
    }

    fn has_unread(&self) -> bool {
        let s = self.state();
        s.total > s.cursor
    }

    fn status(&self) -> JobStatus {
        self.state().status
    }
}

/// Registry of background jobs.
#[derive(Debug, Default)]
pub struct JobManager {
    jobs: Mutex<HashMap<u64, Arc<Job>>>,
    next_id: AtomicU64,
}

impl JobManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// Process-wide manager shared by `run`, `job_output` and `job_kill`.
    pub fn global() -> &'static JobManager {
        static GLOBAL: OnceLock<JobManager> = OnceLock::new();
        GLOBAL.get_or_init(JobManager::new)
    }

    fn jobs(&self) -> MutexGuard<'_, HashMap<u64, Arc<Job>>> {
        self.jobs.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn get(&self, id: u64) -> Result<Arc<Job>, ToolError> {
        self.jobs()
            .get(&id)
            .cloned()
            .ok_or_else(|| ToolError::InvalidArgs(format!("no such job: {id}")))
    }

    /// Spawn `command` as a background job and return its id.
    pub async fn start(
        &self,
        command: &str,
        args: &[String],
        timeout: Option<Duration>,
    ) -> Result<u64, ToolError> {
        let job = {
            let mut jobs = self.jobs();
            let running = jobs.values().filter(|j| j.status().is_running()).count();
            if running >= MAX_RUNNING_JOBS {
                return Err(ToolError::ExecutionFailed(format!(
                    "too many background jobs running ({MAX_RUNNING_JOBS}); stop one with job_kill"
                )));
            }
            let (child, guard, rx, readers) = spawn_piped(command, args)?;
            let id = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
            let mut line = command.to_string();
            for a in args {
                line.push(' ');
                line.push_str(a);
            }
            let job = Arc::new(Job {
                id,
                command: line,
                started: std::time::Instant::now(),
                state: Mutex::new(JobState {
                    status: JobStatus::Running,
                    tail: VecDeque::new(),
                    total: 0,
                    cursor: 0,
                }),
                kill: tokio::sync::Notify::new(),
            });
            prune_finished(&mut jobs);
            jobs.insert(id, job.clone());
            tokio::spawn(supervise(job.clone(), child, guard, rx, readers, timeout));
            job
        };
        Ok(job.id)
    }

    /// Output produced since the last poll. With `wait`, blocks until there is new output or the
    /// job ends (at most `wait`).
    pub async fn output(&self, id: u64, wait: Duration) -> Result<ToolExecutionResult, ToolError> {
        let job = self.get(id)?;
        let deadline = Instant::now() + wait;
        while job.status().is_running() && !job.has_unread() && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let (text, truncated, status) = job.take_output();
        let mut result = ToolExecutionResult::output(format!("{}\n{text}", header(&job, status)));
        result.truncated = truncated;
        if let JobStatus::Exited(code) = status {
            result.exit_code = code;
        }
        Ok(result)
    }

    /// Stop a job's process tree and report its final state.
    pub async fn kill(&self, id: u64) -> Result<ToolExecutionResult, ToolError> {
        let job = self.get(id)?;
        if job.status().is_running() {
            job.kill.notify_one();
            let deadline = Instant::now() + KILL_WAIT;
            while job.status().is_running() && Instant::now() < deadline {
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        }
        let status = job.status();
        if status.is_running() {
            return Err(ToolError::ExecutionFailed(format!(
                "job {id} did not stop within {}s",
                KILL_WAIT.as_secs()
            )));
        }
        Ok(ToolExecutionResult::output(header(&job, status)))
    }

    /// One line per known job.
    pub fn list(&self) -> String {
        let jobs = self.jobs();
        if jobs.is_empty() {
            return "No background jobs.".into();
        }
        let mut all: Vec<_> = jobs.values().cloned().collect();
        all.sort_by_key(|j| j.id);
        all.iter()
            .map(|j| header(j, j.status()))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

fn header(job: &Job, status: JobStatus) -> String {
    format!(
        "job {} [{}] {} ({}s)",
        job.id,
        status.describe(),
        job.command,
        job.started.elapsed().as_secs()
    )
}

/// Forget the oldest finished jobs once more than `MAX_FINISHED_JOBS` have accumulated.
fn prune_finished(jobs: &mut HashMap<u64, Arc<Job>>) {
    let mut finished: Vec<u64> = jobs
        .values()
        .filter(|j| !j.status().is_running())
        .map(|j| j.id)
        .collect();
    if finished.len() < MAX_FINISHED_JOBS {
        return;
    }
    finished.sort_unstable();
    for id in &finished[..finished.len() + 1 - MAX_FINISHED_JOBS] {
        jobs.remove(id);
    }
}

async fn supervise(
    job: Arc<Job>,
    mut child: tokio::process::Child,
    mut guard: TreeGuard,
    mut rx: tokio::sync::mpsc::Receiver<Vec<u8>>,
    readers: [tokio::task::JoinHandle<()>; 2],
    timeout: Option<Duration>,
) {
    // Far enough away to never fire when no timeout was requested.
    let deadline = Instant::now() + timeout.unwrap_or(Duration::from_secs(60 * 60 * 24 * 365));
    let mut rx_open = true;

    let status = loop {
        tokio::select! {
            chunk = rx.recv(), if rx_open => match chunk {
                Some(c) => job.push(&c),
                None => rx_open = false,
            },
            status = child.wait() => {
                break match status {
                    Ok(s) => JobStatus::Exited(s.code()),
                    Err(_) => JobStatus::Exited(None),
                };
            }
            _ = job.kill.notified() => {
                terminate(&mut child, &guard).await;
                break JobStatus::Killed;
            }
            _ = tokio::time::sleep_until(deadline), if timeout.is_some() => {
                terminate(&mut child, &guard).await;
                break JobStatus::TimedOut;
            }
        }
    };
    guard.disarm();

    if rx_open {
        let drain_deadline = Instant::now() + Duration::from_millis(DRAIN_GRACE_MS);
        while let Ok(Some(c)) = timeout_at(drain_deadline, rx.recv()).await {
            job.push(&c);
        }
    }
    for r in &readers {
        r.abort();
    }
    job.finish(status);
}

fn job_id(args: &Value) -> Result<u64, ToolError> {
    args.get("id")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| ToolError::InvalidArgs("missing required integer argument 'id'".into()))
}

/// `job_output` — read new output from a background job (or list jobs when `id` is omitted).
#[derive(Debug, Default)]
pub struct JobOutputTool;

impl JobOutputTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Tool for JobOutputTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "job_output",
            "Read output a background job (started with run background:true) produced since the last call, plus its status. Omit id to list all jobs.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "id": {"type": "integer", "description": "Job id returned by run."},
                    "wait_secs": {
                        "type": "integer",
                        "minimum": 0,
                        "maximum": 60,
                        "description": "Wait up to this long for new output or exit (default 0)."
                    }
                }
            }),
        )
        .with_category(ToolCategory::Read)
    }

    async fn execute(&self, args: Value) -> Result<ToolExecutionResult, ToolError> {
        let manager = JobManager::global();
        if args.get("id").is_none() {
            return Ok(ToolExecutionResult::output(manager.list()));
        }
        let wait = args
            .get("wait_secs")
            .and_then(|v| v.as_u64())
            .unwrap_or(0)
            .min(MAX_WAIT_SECS);
        manager
            .output(job_id(&args)?, Duration::from_secs(wait))
            .await
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// `job_kill` — stop a background job and its process tree.
#[derive(Debug, Default)]
pub struct JobKillTool;

impl JobKillTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Tool for JobKillTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "job_kill",
            "Stop a background job and its whole process tree.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "id": {"type": "integer", "description": "Job id returned by run."}
                },
                "required": ["id"]
            }),
        )
        .with_category(ToolCategory::Network)
    }

    async fn execute(&self, args: Value) -> Result<ToolExecutionResult, ToolError> {
        JobManager::global().kill(job_id(&args)?).await
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn shell(script: &str) -> (String, Vec<String>) {
        if cfg!(windows) {
            ("cmd".into(), vec!["/C".into(), script.into()])
        } else {
            ("sh".into(), vec!["-c".into(), script.into()])
        }
    }

    async fn start(m: &JobManager, script: &str, timeout: Option<Duration>) -> u64 {
        let (cmd, args) = shell(script);
        m.start(&cmd, &args, timeout).await.unwrap()
    }

    #[tokio::test]
    async fn background_run_returns_immediately_and_output_is_incremental() {
        let m = JobManager::new();
        let script = if cfg!(windows) {
            "echo first & ping -n 3 127.0.0.1 >nul & echo second"
        } else {
            "echo first; sleep 1; echo second"
        };
        let id = start(&m, script, None).await;
        assert!(m.list().contains("[running]"));

        let r = m.output(id, Duration::from_secs(5)).await.unwrap();
        assert!(r.output.contains("first"), "{}", r.output);
        assert!(r.output.contains("[running]"), "{}", r.output);

        let r = m.output(id, Duration::from_secs(10)).await.unwrap();
        assert!(r.output.contains("second"), "{}", r.output);
        let body = r.output.split_once('\n').map_or("", |(_, b)| b);
        assert!(!body.contains("first"), "poll must be incremental");

        let mut r = m.output(id, Duration::from_secs(5)).await.unwrap();
        for _ in 0..50 {
            if r.exit_code.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
            r = m.output(id, Duration::ZERO).await.unwrap();
        }
        assert_eq!(r.exit_code, Some(0), "{}", r.output);
        assert!(r.output.contains("exited with code 0"));
    }

    #[tokio::test]
    async fn kill_stops_a_running_job() {
        let m = JobManager::new();
        let script = if cfg!(windows) {
            "ping -n 100 127.0.0.1"
        } else {
            "sleep 100"
        };
        let id = start(&m, script, None).await;
        let r = m.kill(id).await.unwrap();
        assert!(r.output.contains("[killed]"), "{}", r.output);
        assert!(m.list().contains("[killed]"));
    }

    #[tokio::test]
    async fn explicit_timeout_kills_the_job() {
        let m = JobManager::new();
        let script = if cfg!(windows) {
            "ping -n 100 127.0.0.1"
        } else {
            "sleep 100"
        };
        let id = start(&m, script, Some(Duration::from_secs(1))).await;
        tokio::time::sleep(Duration::from_secs(4)).await;
        let r = m.output(id, Duration::ZERO).await.unwrap();
        assert!(r.output.contains("timed out"), "{}", r.output);
    }

    #[tokio::test]
    async fn unknown_job_and_job_limit_are_errors() {
        let m = JobManager::new();
        assert!(m.output(99, Duration::ZERO).await.is_err());
        assert!(m.kill(99).await.is_err());

        let script = if cfg!(windows) {
            "ping -n 100 127.0.0.1"
        } else {
            "sleep 100"
        };
        let mut ids = Vec::new();
        for _ in 0..MAX_RUNNING_JOBS {
            ids.push(start(&m, script, None).await);
        }
        let (cmd, args) = shell(script);
        let err = m.start(&cmd, &args, None).await.unwrap_err();
        assert!(err.to_string().contains("too many background jobs"));
        for id in ids {
            m.kill(id).await.unwrap();
        }
    }

    #[test]
    fn take_output_reports_dropped_bytes() {
        let job = Job {
            id: 1,
            command: "x".into(),
            started: std::time::Instant::now(),
            state: Mutex::new(JobState {
                status: JobStatus::Running,
                tail: VecDeque::new(),
                total: 0,
                cursor: 0,
            }),
            kill: tokio::sync::Notify::new(),
        };
        job.push(&vec![b'a'; CAPTURE_PART]);
        job.push(b"END");
        let (text, _, _) = job.take_output();
        assert!(text.contains("bytes of earlier output dropped"));
        assert!(text.contains("END"));
        assert!(job.take_output().0.is_empty());
    }

    #[tokio::test]
    async fn tools_validate_arguments() {
        let err = JobKillTool::new().execute(json!({})).await.unwrap_err();
        assert!(err.to_string().contains("'id'"));
        let list = JobOutputTool::new().execute(json!({})).await.unwrap();
        assert!(!list.output.is_empty());
    }
}
