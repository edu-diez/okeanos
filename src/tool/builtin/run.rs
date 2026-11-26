//! Built-in `run` tool — execute a local process (no shell) with bounded, ordered output.
//!
//! Category: Network (requires approval — local process execution is the permission boundary;
//! `Policy::check_call` scopes session grants and allow-list rules to the command).
//!
//! Behaviour:
//! - Default timeout of 120s (`timeout_secs`, 1..=3600) so a hung command never hangs the agent.
//! - stdout and stderr are merged into one stream in arrival order. Raw bytes are read, so
//!   non-UTF-8 output is decoded lossily instead of stalling the pipe.
//! - Capture memory is bounded: the first and last `CAPTURE_PART` bytes are kept, with the
//!   omitted byte count noted. The result is then truncated to `MAX_OUTPUT_BYTES` with the full
//!   captured text spilled to a temp file.
//! - Completion is detected from process exit, not pipe EOF; a grandchild holding the pipe open
//!   delays the result by at most `DRAIN_GRACE_MS`.
//! - On timeout the whole process tree is killed (Unix: SIGTERM → grace → SIGKILL on the process
//!   group; Windows: Job Object) and the output produced so far is returned with
//!   `timed_out: true`. If the future is dropped (cancellation) the tree is killed as well.
//! - stdin is closed and secret-looking environment variables are not passed to the child
//!   (set `OKEANOS_RUN_INHERIT_ENV=1` to disable scrubbing).

use crate::tool::builtin::{MAX_OUTPUT_BYTES, truncate_output_with_spill};
use crate::tool::tool_trait::{Tool, ToolError, ToolExecutionResult, ToolOutputSink, ToolSpec};
use async_trait::async_trait;
use serde_json::Value;
use std::collections::VecDeque;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;
use tokio::time::{Instant, timeout_at};

#[cfg(unix)]
const GRACE_PERIOD_MS: u64 = 500; // Between SIGTERM and SIGKILL
const DRAIN_GRACE_MS: u64 = 250; // Max extra read time after the process exits
const DEFAULT_TIMEOUT_SECS: u64 = 120;
const MAX_TIMEOUT_SECS: u64 = 3600;
const CAPTURE_PART: usize = 256 * 1024;
const LIVE_CAP: usize = 256 * 1024; // Max bytes streamed per call

/// Built-in tool for executing local commands.
#[derive(Debug, Default)]
pub struct RunTool;

impl RunTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Tool for RunTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "run",
            "Execute a local command (no shell) with a timeout (default 120s). stdin is closed.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "Command to execute."
                    },
                    "args": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "Command-line arguments."
                    },
                    "timeout_secs": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 3600,
                        "description": "Maximum seconds to wait before killing the process tree (default 120)."
                    }
                },
                "required": ["command"]
            }),
        )
        .with_category(crate::tool::tool_trait::ToolCategory::Network)
    }

    async fn execute(&self, args: Value) -> Result<ToolExecutionResult, ToolError> {
        self.execute_streaming(args, ToolOutputSink::discard())
            .await
    }

    async fn execute_streaming(
        &self,
        args: Value,
        sink: ToolOutputSink,
    ) -> Result<ToolExecutionResult, ToolError> {
        let command = args
            .get("command")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArgs("missing required argument 'command'".into()))?;

        let args_vec: Vec<String> = args
            .get("args")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();

        let timeout_secs = args
            .get("timeout_secs")
            .and_then(|v| v.as_u64())
            .unwrap_or(DEFAULT_TIMEOUT_SECS)
            .clamp(1, MAX_TIMEOUT_SECS);

        crate::tool::workspace::guard_run(command, &args_vec)?;
        let wrapped = crate::tool::sandbox::apply(command, &args_vec)?;
        let mut cmd = Command::new(&wrapped.program);
        cmd.args(&wrapped.args);
        cmd.stdin(Stdio::null());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        scrub_env(&mut cmd);

        // Backstop in case the guard below cannot run.
        cmd.kill_on_drop(true);

        #[cfg(unix)]
        {
            // Own process group so the whole tree can be signalled.
            cmd.process_group(0);
        }

        let mut child = cmd
            .spawn()
            .map_err(|e| ToolError::ExecutionFailed(format!("failed to spawn '{command}': {e}")))?;

        // Kills the whole tree if this future is dropped (cancellation).
        let mut guard = TreeGuard::new(&child);

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| ToolError::ExecutionFailed("failed to capture stdout".into()))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| ToolError::ExecutionFailed("failed to capture stderr".into()))?;

        // Both streams feed one channel so output keeps its arrival order.
        let (tx, mut rx) = mpsc::channel::<Vec<u8>>(64);
        let readers = [
            tokio::spawn(pump(stdout, tx.clone())),
            tokio::spawn(pump(stderr, tx)),
        ];

        let mut capture = Capture::default();
        let mut live = LiveOutput::new(sink);
        let deadline = Instant::now() + Duration::from_secs(timeout_secs);
        let mut rx_open = true;

        let status = loop {
            tokio::select! {
                chunk = rx.recv(), if rx_open => match chunk {
                    Some(c) => {
                        live.push(&c);
                        capture.push(&c);
                    }
                    None => rx_open = false,
                },
                status = child.wait() => break Some(status),
                _ = tokio::time::sleep_until(deadline) => break None,
            }
        };

        let (exit_code, timed_out) = match status {
            Some(Ok(status)) => (status.code(), false),
            Some(Err(e)) => {
                return Err(ToolError::ExecutionFailed(format!(
                    "process wait failed: {e}"
                )));
            }
            None => {
                terminate(&mut child, &guard).await;
                (None, true)
            }
        };
        guard.disarm();

        // Collect what is left in the pipes, bounded in time so a grandchild
        // that inherited the pipe cannot stall the result.
        if rx_open {
            let drain_deadline = Instant::now() + Duration::from_millis(DRAIN_GRACE_MS);
            while let Ok(Some(c)) = timeout_at(drain_deadline, rx.recv()).await {
                live.push(&c);
                capture.push(&c);
            }
        }
        live.finish();
        for r in &readers {
            r.abort();
        }

        let mut output = capture.finish();
        if timed_out {
            output.push_str(&format!(
                "\n[timed out after {timeout_secs}s; process tree killed]"
            ));
        }
        let (output, truncated) = truncate_output_with_spill(&output, MAX_OUTPUT_BYTES);

        Ok(ToolExecutionResult {
            output,
            exit_code,
            timed_out,
            cancelled: false,
            truncated,
        })
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// Forward raw chunks from a stream until EOF or the receiver goes away.
async fn pump(mut stream: impl AsyncRead + Unpin + Send + 'static, tx: mpsc::Sender<Vec<u8>>) {
    let mut buf = [0u8; 8192];
    loop {
        match stream.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if tx.send(buf[..n].to_vec()).await.is_err() {
                    break;
                }
            }
        }
    }
}

/// Streams captured bytes to the output sink as UTF-8 text, up to `LIVE_CAP` bytes.
struct LiveOutput {
    sink: ToolOutputSink,
    pending: Vec<u8>,
    sent: usize,
    capped: bool,
}

impl LiveOutput {
    fn new(sink: ToolOutputSink) -> Self {
        Self {
            sink,
            pending: Vec::new(),
            sent: 0,
            capped: false,
        }
    }

    fn push(&mut self, chunk: &[u8]) {
        if self.capped || !self.sink.is_active() {
            return;
        }
        let text = decode_utf8(&mut self.pending, chunk, false);
        self.emit(text);
    }

    fn finish(&mut self) {
        if self.capped || !self.sink.is_active() {
            return;
        }
        let text = decode_utf8(&mut self.pending, &[], true);
        self.emit(text);
    }

    fn emit(&mut self, text: String) {
        if text.is_empty() {
            return;
        }
        self.sent += text.len();
        self.sink.send(text);
        if self.sent >= LIVE_CAP {
            self.capped = true;
            self.sink
                .send("\n[live output truncated; the final result has the full head/tail]\n");
        }
    }
}

/// Decode `chunk` appended to `pending`, keeping an incomplete trailing sequence for the next
/// call (or replacing it when `flush` is set). Invalid bytes become U+FFFD.
fn decode_utf8(pending: &mut Vec<u8>, chunk: &[u8], flush: bool) -> String {
    pending.extend_from_slice(chunk);
    let mut out = String::new();
    let mut start = 0;
    loop {
        match std::str::from_utf8(&pending[start..]) {
            Ok(s) => {
                out.push_str(s);
                start = pending.len();
                break;
            }
            Err(e) => {
                let valid = e.valid_up_to();
                out.push_str(&String::from_utf8_lossy(&pending[start..start + valid]));
                start += valid;
                match e.error_len() {
                    Some(n) => {
                        out.push('\u{FFFD}');
                        start += n;
                    }
                    None => {
                        if flush {
                            out.push('\u{FFFD}');
                            start = pending.len();
                        }
                        break;
                    }
                }
            }
        }
    }
    pending.drain(..start);
    out
}

/// Bounded output capture: keeps the head and the most recent tail.
#[derive(Default)]
struct Capture {
    head: Vec<u8>,
    tail: VecDeque<u8>,
    omitted: usize,
}

impl Capture {
    fn push(&mut self, mut chunk: &[u8]) {
        if self.head.len() < CAPTURE_PART {
            let take = (CAPTURE_PART - self.head.len()).min(chunk.len());
            self.head.extend_from_slice(&chunk[..take]);
            chunk = &chunk[take..];
        }
        self.tail.extend(chunk);
        if self.tail.len() > CAPTURE_PART {
            let excess = self.tail.len() - CAPTURE_PART;
            self.tail.drain(..excess);
            self.omitted += excess;
        }
    }

    fn finish(self) -> String {
        let mut bytes = self.head;
        if self.omitted > 0 {
            bytes.extend_from_slice(
                format!("\n...[{} bytes omitted]...\n", self.omitted).as_bytes(),
            );
        }
        bytes.extend(self.tail);
        String::from_utf8_lossy(&bytes).into_owned()
    }
}

/// Owns the child's process tree (Unix process group / Windows Job Object).
/// Dropping it while armed — e.g. on cancellation — kills the whole tree.
struct TreeGuard {
    armed: bool,
    #[cfg(unix)]
    pgid: Option<u32>,
    #[cfg(windows)]
    job: Option<job::Job>,
}

impl TreeGuard {
    fn new(child: &Child) -> Self {
        Self {
            armed: true,
            #[cfg(unix)]
            pgid: child.id(),
            #[cfg(windows)]
            job: child.raw_handle().and_then(job::Job::containing),
        }
    }

    fn kill(&self) {
        #[cfg(unix)]
        if let Some(pgid) = self.pgid {
            use nix::sys::signal::{Signal, killpg};
            use nix::unistd::Pid;
            let _ = killpg(Pid::from_raw(pgid as i32), Signal::SIGKILL);
        }
        #[cfg(windows)]
        if let Some(job) = &self.job {
            job.terminate();
        }
    }

    /// Normal completion: leave any background processes the command started running.
    fn disarm(&mut self) {
        self.armed = false;
        #[cfg(windows)]
        if let Some(job) = &self.job {
            job.release();
        }
    }
}

impl Drop for TreeGuard {
    fn drop(&mut self) {
        if self.armed {
            self.kill();
        }
    }
}

#[cfg(windows)]
mod job {
    use std::ffi::c_void;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject, TerminateJobObject,
    };

    /// A Job Object with kill-on-close semantics holding the child tree.
    pub struct Job(HANDLE);

    // The handle is only used through thread-safe Win32 calls.
    unsafe impl Send for Job {}
    unsafe impl Sync for Job {}

    impl Job {
        /// Create a job and assign `process` to it. Descendants spawned afterwards inherit it.
        pub fn containing(process: *mut c_void) -> Option<Job> {
            // SAFETY: plain Win32 calls on handles we own; `info` is a valid zeroed struct.
            unsafe {
                let handle = CreateJobObjectW(std::ptr::null(), std::ptr::null());
                if handle.is_null() {
                    return None;
                }
                let job = Job(handle);
                if !job.set_kill_on_close(true) || AssignProcessToJobObject(handle, process) == 0 {
                    return None;
                }
                Some(job)
            }
        }

        fn set_kill_on_close(&self, on: bool) -> bool {
            // SAFETY: see `containing`.
            unsafe {
                let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
                if on {
                    info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                }
                SetInformationJobObject(
                    self.0,
                    JobObjectExtendedLimitInformation,
                    &info as *const _ as *const c_void,
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                ) != 0
            }
        }

        pub fn terminate(&self) {
            // SAFETY: valid job handle.
            unsafe {
                TerminateJobObject(self.0, 1);
            }
        }

        /// Stop killing the tree when the handle is closed.
        pub fn release(&self) {
            self.set_kill_on_close(false);
        }
    }

    impl Drop for Job {
        fn drop(&mut self) {
            // SAFETY: closing the handle we own exactly once.
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
}

/// Stop the process tree after a timeout and reap the leader.
async fn terminate(child: &mut Child, guard: &TreeGuard) {
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        use nix::sys::signal::{Signal, killpg};
        use nix::unistd::Pid;
        let _ = killpg(Pid::from_raw(pid as i32), Signal::SIGTERM);
        tokio::time::sleep(Duration::from_millis(GRACE_PERIOD_MS)).await;
    }
    // The leader is not reaped yet, so its pid/pgid cannot have been reused.
    guard.kill();
    let _ = child.kill().await;
}

/// Remove credentials from the child's environment.
fn scrub_env(cmd: &mut Command) {
    if std::env::var("OKEANOS_RUN_INHERIT_ENV").is_ok_and(|v| v == "1") {
        return;
    }
    for (name, _) in std::env::vars_os() {
        if let Some(n) = name.to_str()
            && is_secret_var(n)
        {
            cmd.env_remove(n);
        }
    }
}

fn is_secret_var(name: &str) -> bool {
    let n = name.to_ascii_uppercase();
    const PREFIXES: &[&str] = &[
        "OKEANOS_",
        "ANTHROPIC_",
        "OPENAI_",
        "GEMINI_",
        "AZURE_OPENAI_",
    ];
    const EXACT: &[&str] = &[
        "GITHUB_TOKEN",
        "GH_TOKEN",
        "AWS_ACCESS_KEY_ID",
        "AWS_SESSION_TOKEN",
        "GOOGLE_API_KEY",
    ];
    const SUFFIXES: &[&str] = &[
        "_API_KEY",
        "_APIKEY",
        "_TOKEN",
        "_SECRET",
        "_PASSWORD",
        "_PRIVATE_KEY",
        "_ACCESS_KEY",
    ];
    const CONTAINS: &[&str] = &["PASSWORD", "SECRET", "CREDENTIAL"];
    PREFIXES.iter().any(|p| n.starts_with(p))
        || EXACT.contains(&n.as_str())
        || SUFFIXES.iter().any(|s| n.ends_with(s))
        || CONTAINS.iter().any(|c| n.contains(c))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Run a shell script portably: `sh -c` on Unix, `cmd /C` on Windows.
    fn shell(script: &str) -> Value {
        if cfg!(windows) {
            json!({ "command": "cmd", "args": ["/C", script] })
        } else {
            json!({ "command": "sh", "args": ["-c", script] })
        }
    }

    #[tokio::test]
    async fn test_run_tool_basic() {
        let tool = RunTool::new();
        let result = tool.execute(shell("echo hello world")).await.unwrap();

        assert!(!result.timed_out);
        assert!(!result.cancelled);
        assert_eq!(result.exit_code, Some(0));
        assert!(result.output.contains("hello world"));
    }

    #[tokio::test]
    async fn test_run_tool_timeout_keeps_output_and_kills_tree() {
        let tool = RunTool::new();
        let script = if cfg!(windows) {
            "echo before & ping -n 100 127.0.0.1"
        } else {
            "echo before; sleep 100"
        };
        let mut args = shell(script);
        args["timeout_secs"] = json!(3);
        let start = std::time::Instant::now();
        let result = tool.execute(args).await.unwrap();

        assert!(result.timed_out);
        assert!(result.exit_code.is_none());
        assert!(result.output.contains("before"), "got: {}", result.output);
        assert!(start.elapsed() < Duration::from_secs(15));
    }

    #[tokio::test]
    async fn test_run_tool_missing_command() {
        let tool = RunTool::new();
        let result = tool.execute(json!({})).await;
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("missing required argument 'command'")
        );
    }

    #[tokio::test]
    async fn test_run_tool_nonexistent_command() {
        let tool = RunTool::new();
        let result = tool
            .execute(json!({
                "command": "okeanos-nonexistent-command-12345"
            }))
            .await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_run_tool_non_zero_exit() {
        let tool = RunTool::new();
        let result = tool.execute(shell("exit 42")).await.unwrap();

        assert_eq!(result.exit_code, Some(42));
        assert!(!result.timed_out);
    }

    #[tokio::test]
    async fn test_run_tool_stdin_is_closed() {
        let tool = RunTool::new();
        let script = if cfg!(windows) { "more" } else { "cat" };
        let result = tool.execute(shell(script)).await.unwrap();
        assert!(!result.timed_out);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_run_tool_stderr_captured() {
        let tool = RunTool::new();
        let result = tool.execute(shell("echo error >&2")).await.unwrap();

        assert_eq!(result.exit_code, Some(0));
        assert!(result.output.contains("error"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_run_tool_invalid_utf8_does_not_stall() {
        let tool = RunTool::new();
        let result = tool
            .execute(shell(
                "printf '\\377\\376 start\\n'; yes x | head -c 200000",
            ))
            .await
            .unwrap();
        assert_eq!(result.exit_code, Some(0));
        assert!(result.output.contains("start"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_run_tool_background_grandchild_does_not_hang() {
        let tool = RunTool::new();
        let start = std::time::Instant::now();
        let result = tool.execute(shell("sleep 30 & echo done")).await.unwrap();
        assert_eq!(result.exit_code, Some(0));
        assert!(result.output.contains("done"));
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn test_decode_utf8_handles_split_sequences() {
        let bytes = "héllo €".as_bytes();
        let mut pending = Vec::new();
        let mut out = String::new();
        for b in bytes {
            out.push_str(&decode_utf8(&mut pending, &[*b], false));
        }
        out.push_str(&decode_utf8(&mut pending, &[], true));
        assert_eq!(out, "héllo €");

        let mut pending = Vec::new();
        let mut out = decode_utf8(&mut pending, &[b'a', 0xFF, b'b', 0xE2, 0x82], false);
        out.push_str(&decode_utf8(&mut pending, &[], true));
        assert_eq!(out, "a\u{FFFD}b\u{FFFD}");
    }

    #[tokio::test]
    async fn test_run_tool_streams_output_and_keeps_final_result() {
        let tool = RunTool::new();
        let (sink, mut rx) = ToolOutputSink::channel();
        let result = tool
            .execute_streaming(shell("echo first && echo second"), sink)
            .await
            .unwrap();
        let mut streamed = String::new();
        while let Ok(chunk) = rx.try_recv() {
            streamed.push_str(&chunk);
        }
        assert!(streamed.contains("first") && streamed.contains("second"));
        assert!(streamed.find("first") < streamed.find("second"));
        assert_eq!(result.exit_code, Some(0));
        assert!(result.output.contains("first") && result.output.contains("second"));
    }

    #[test]
    fn test_capture_bounds_memory_and_notes_omission() {
        let mut c = Capture::default();
        let chunk = vec![b'a'; 100_000];
        for _ in 0..20 {
            c.push(&chunk);
        }
        c.push(b"END");
        assert!(c.head.len() <= CAPTURE_PART && c.tail.len() <= CAPTURE_PART);
        let text = c.finish();
        assert!(text.contains("bytes omitted"));
        assert!(text.ends_with("END"));
        assert!(text.len() < 2 * CAPTURE_PART + 100);
    }

    #[test]
    fn test_is_secret_var() {
        for v in [
            "OPENAI_API_KEY",
            "github_token",
            "DB_PASSWORD",
            "OKEANOS_X",
            "AWS_SECRET_ACCESS_KEY",
        ] {
            assert!(is_secret_var(v), "{v}");
        }
        for v in ["PATH", "HOME", "CARGO_HOME", "TERM", "SystemRoot"] {
            assert!(!is_secret_var(v), "{v}");
        }
    }
}
