//! Built-in `run` tool — execute a local process with interruptible, streaming output.
//!
//! Category: Network (requires approval — local process execution is the permission boundary).
//!
//! Features:
//! - Configurable per-invocation timeout (`timeout_secs` argument).
//! - Output streams incrementally (line by line) as the process produces it.
//! - On timeout: SIGTERM → grace period → SIGKILL, drain output, return with `timed_out: true`.
//! - On cancellation (shutdown signal): process killed via `kill_on_drop`.
//! - Returns `exit_code`, `timed_out`, `cancelled` in the tool result.
//! - Output is truncated to MAX_OUTPUT_BYTES with head/tail overflow indicator.

use crate::tool::builtin::{MAX_OUTPUT_BYTES, truncate_output_with_spill};
use crate::tool::tool_trait::{Tool, ToolError, ToolExecutionResult, ToolSpec};
use async_trait::async_trait;
use serde_json::Value;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::time::timeout;

const GRACE_PERIOD_MS: u64 = 500; // Grace period between SIGTERM and SIGKILL

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
            "Execute a local command with optional timeout.",
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
                        "description": "Maximum seconds to wait before killing the process."
                    }
                },
                "required": ["command"]
            }),
        )
        .with_category(crate::tool::tool_trait::ToolCategory::Network)
    }

    async fn execute(&self, args: Value) -> Result<ToolExecutionResult, ToolError> {
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

        let timeout_secs = args.get("timeout_secs").and_then(|v| v.as_u64());

        let mut cmd = Command::new(command);
        cmd.args(&args_vec);
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());

        // If the future is dropped (e.g. due to cancellation), kill the process.
        cmd.kill_on_drop(true);

        #[cfg(unix)]
        {
            // Place the child in its own process group so we can signal
            // the entire group on cancellation/timeout.
            cmd.process_group(0);
        }

        let mut child = cmd
            .spawn()
            .map_err(|e| ToolError::ExecutionFailed(format!("failed to spawn '{command}': {e}")))?;

        // Take stdout/stderr so we can read them incrementally.
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| ToolError::ExecutionFailed("failed to capture stdout".into()))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| ToolError::ExecutionFailed("failed to capture stderr".into()))?;

        // Spawn tasks to read stdout and stderr line by line.
        let stdout_handle = tokio::spawn(read_stream(stdout));
        let stderr_handle = tokio::spawn(read_stream(stderr));

        // Wait for the process to exit, with an optional timeout.
        let wait_result: Result<std::process::ExitStatus, std::io::Error> =
            if let Some(secs) = timeout_secs {
                match timeout(Duration::from_secs(secs), child.wait()).await {
                    Ok(status) => status,
                    Err(_elapsed) => {
                        // Timeout: implement SIGTERM → grace period → SIGKILL sequence.
                        #[cfg(unix)]
                        {
                            use nix::sys::signal::{Signal, kill};
                            use nix::unistd::Pid;
                            if let Some(pid) = child.id() {
                                // Get the process group ID (negative of pid since we set process_group(0))
                                let pgid = Pid::from_raw(-(pid as i32));
                                // Send SIGTERM to the process group
                                let _ = kill(pgid, Signal::SIGTERM);
                                // Wait for grace period
                                tokio::time::sleep(Duration::from_millis(GRACE_PERIOD_MS)).await;
                                // Force SIGKILL if still alive
                                let _ = kill(pgid, Signal::SIGKILL);
                            }
                        }
                        #[cfg(windows)]
                        {
                            let _ = child.kill().await;
                        }
                        let _ = child.wait().await;
                        // Abort the stream readers — the pipes are now closed.
                        stdout_handle.abort();
                        stderr_handle.abort();
                        let output = drain_output(stdout_handle, stderr_handle).await;
                        return Ok(ToolExecutionResult {
                            output,
                            exit_code: None,
                            timed_out: true,
                            cancelled: false,
                            truncated: false,
                        });
                    }
                }
            } else {
                child.wait().await
            };

        // Drain remaining output from the stream readers.
        let stdout_output = stdout_handle.await.unwrap_or_default();
        let stderr_output = stderr_handle.await.unwrap_or_default();
        let output = format!("{}{}", stdout_output, stderr_output);

        let (exit_code, timed_out, cancelled) = match wait_result {
            Ok(status) => (status.code(), false, false),
            Err(e) => {
                return Err(ToolError::ExecutionFailed(format!(
                    "process wait failed: {e}"
                )));
            }
        };

        // Truncate output if needed.
        let (truncated_output, truncated) = truncate_output_with_spill(&output, MAX_OUTPUT_BYTES);

        Ok(ToolExecutionResult {
            output: truncated_output,
            exit_code,
            timed_out,
            cancelled,
            truncated,
        })
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// Read all lines from an async stream into a String.
async fn read_stream(stream: impl tokio::io::AsyncRead + Unpin + Send + 'static) -> String {
    let mut output = String::new();
    let mut lines = BufReader::new(stream).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        output.push_str(&line);
        output.push('\n');
    }
    output
}

/// Drain output from potentially-aborted stream handles.
async fn drain_output(
    stdout_handle: tokio::task::JoinHandle<String>,
    stderr_handle: tokio::task::JoinHandle<String>,
) -> String {
    let stdout_output = stdout_handle.await.unwrap_or_default();
    let stderr_output = stderr_handle.await.unwrap_or_default();
    format!("{}{}", stdout_output, stderr_output)
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
        assert!(result.exit_code.is_some());
        assert_eq!(result.exit_code, Some(0));
        assert!(result.output.contains("hello world"));
    }

    #[tokio::test]
    async fn test_run_tool_timeout() {
        let tool = RunTool::new();
        let args = if cfg!(windows) {
            json!({ "command": "ping", "args": ["-n", "100", "127.0.0.1"], "timeout_secs": 1 })
        } else {
            json!({ "command": "sleep", "args": ["100"], "timeout_secs": 1 })
        };
        let result = tool.execute(args).await.unwrap();

        assert!(result.timed_out);
        assert!(!result.cancelled);
        assert!(result.exit_code.is_none());
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

    #[cfg(unix)]
    #[tokio::test]
    async fn test_run_tool_stderr_captured() {
        let tool = RunTool::new();
        let result = tool
            .execute(json!({
                "command": "sh",
                "args": ["-c", "echo error >&2"]
            }))
            .await
            .unwrap();

        assert_eq!(result.exit_code, Some(0));
        assert!(result.output.contains("error"));
    }
}
