//! Lifecycle hooks: user commands run around tool calls.
//!
//! Configured with `[[hooks]]` tables:
//!
//! ```toml
//! [[hooks]]
//! event = "PreToolUse"        # or "PostToolUse"
//! matcher = "^(write|edit)$"  # optional regex on the tool name
//! command = "./scripts/check.sh"
//! timeout_secs = 10           # optional, default 10
//! ```
//!
//! The hook receives a JSON object on stdin (`event`, `tool_name`,
//! `tool_input`, and for PostToolUse `tool_output`). Exit code `0` continues.
//! Exit code `2` blocks a PreToolUse call (stderr becomes the reason) or, for
//! PostToolUse, feeds stderr back to the model. Any other non-zero exit is
//! logged and ignored (fail-open for broken hooks).

use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

const DEFAULT_TIMEOUT_SECS: u64 = 10;
const MAX_FEEDBACK_BYTES: usize = 4_000;

/// Hook trigger point.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub enum HookEvent {
    PreToolUse,
    PostToolUse,
}

impl HookEvent {
    fn as_str(self) -> &'static str {
        match self {
            Self::PreToolUse => "PreToolUse",
            Self::PostToolUse => "PostToolUse",
        }
    }
}

/// One `[[hooks]]` entry.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct HookConfig {
    pub event: HookEvent,
    /// Regex matched against the tool name; absent matches every tool.
    #[serde(default)]
    pub matcher: Option<String>,
    pub command: String,
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

/// Result of PreToolUse hooks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreToolOutcome {
    Continue,
    Block(String),
}

struct Hook {
    event: HookEvent,
    matcher: Option<Regex>,
    command: String,
    timeout: Duration,
}

/// Compiled set of hooks.
#[derive(Default)]
pub struct Hooks {
    hooks: Vec<Hook>,
    cwd: Option<PathBuf>,
}

impl std::fmt::Debug for Hooks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Hooks")
            .field("count", &self.hooks.len())
            .finish()
    }
}

struct HookRun {
    code: Option<i32>,
    stderr: String,
}

impl Hooks {
    /// Compile hook configs; invalid matcher regexes are returned as errors.
    pub fn new(configs: &[HookConfig], cwd: Option<PathBuf>) -> Result<Self, String> {
        let mut hooks = Vec::new();
        for c in configs {
            let matcher = match &c.matcher {
                Some(m) => {
                    Some(Regex::new(m).map_err(|e| format!("invalid hook matcher '{m}': {e}"))?)
                }
                None => None,
            };
            hooks.push(Hook {
                event: c.event,
                matcher,
                command: c.command.clone(),
                timeout: Duration::from_secs(c.timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS)),
            });
        }
        Ok(Self { hooks, cwd })
    }

    pub fn is_empty(&self) -> bool {
        self.hooks.is_empty()
    }

    fn matching(&self, event: HookEvent, tool: &str) -> impl Iterator<Item = &Hook> {
        self.hooks.iter().filter(move |h| {
            h.event == event && h.matcher.as_ref().is_none_or(|m| m.is_match(tool))
        })
    }

    /// Run PreToolUse hooks; the first one exiting with code 2 blocks the call.
    pub async fn pre_tool_use(&self, tool: &str, input: &Value) -> PreToolOutcome {
        for hook in self.matching(HookEvent::PreToolUse, tool) {
            let payload = json!({
                "event": HookEvent::PreToolUse.as_str(),
                "tool_name": tool,
                "tool_input": input,
            });
            if let Some(run) = self.run(hook, &payload).await
                && run.code == Some(2)
            {
                let reason = if run.stderr.trim().is_empty() {
                    "blocked by PreToolUse hook".to_string()
                } else {
                    run.stderr.trim().to_string()
                };
                return PreToolOutcome::Block(reason);
            }
        }
        PreToolOutcome::Continue
    }

    /// Run PostToolUse hooks; returns stderr feedback from hooks exiting with 2.
    pub async fn post_tool_use(&self, tool: &str, input: &Value, output: &str) -> Option<String> {
        let mut feedback = Vec::new();
        for hook in self.matching(HookEvent::PostToolUse, tool) {
            let payload = json!({
                "event": HookEvent::PostToolUse.as_str(),
                "tool_name": tool,
                "tool_input": input,
                "tool_output": output,
            });
            if let Some(run) = self.run(hook, &payload).await
                && run.code == Some(2)
                && !run.stderr.trim().is_empty()
            {
                feedback.push(run.stderr.trim().to_string());
            }
        }
        if feedback.is_empty() {
            None
        } else {
            let mut text = feedback.join("\n");
            text.truncate(text.floor_char_boundary(MAX_FEEDBACK_BYTES));
            Some(text)
        }
    }

    /// Run one hook. `None` when it could not be run or timed out.
    async fn run(&self, hook: &Hook, payload: &Value) -> Option<HookRun> {
        let mut cmd = if cfg!(windows) {
            let mut c = Command::new("cmd");
            c.args(["/C", &hook.command]);
            c
        } else {
            let mut c = Command::new("sh");
            c.args(["-c", &hook.command]);
            c
        };
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some(cwd) = &self.cwd {
            cmd.current_dir(cwd);
        }
        let mut child = cmd.spawn().ok()?;
        if let Some(mut stdin) = child.stdin.take() {
            // A hook that ignores stdin may close it early; that is fine.
            let _ = stdin.write_all(payload.to_string().as_bytes()).await;
        }
        let out = tokio::time::timeout(hook.timeout, child.wait_with_output())
            .await
            .ok()?
            .ok()?;
        Some(HookRun {
            code: out.status.code(),
            stderr: String::from_utf8_lossy(&out.stderr).to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(event: HookEvent, matcher: Option<&str>, command: &str) -> HookConfig {
        HookConfig {
            event,
            matcher: matcher.map(String::from),
            command: command.to_string(),
            timeout_secs: Some(5),
        }
    }

    #[tokio::test]
    async fn exit_code_two_blocks_with_stderr_reason() {
        let hooks = Hooks::new(
            &[cfg(
                HookEvent::PreToolUse,
                Some("^run$"),
                "echo nope 1>&2 && exit 2",
            )],
            None,
        )
        .unwrap();
        assert_eq!(
            hooks.pre_tool_use("run", &json!({})).await,
            PreToolOutcome::Block("nope".to_string())
        );
        // Non-matching tool is unaffected.
        assert_eq!(
            hooks.pre_tool_use("read", &json!({})).await,
            PreToolOutcome::Continue
        );
    }

    #[tokio::test]
    async fn other_failures_are_ignored() {
        let hooks = Hooks::new(&[cfg(HookEvent::PreToolUse, None, "exit 1")], None).unwrap();
        assert_eq!(
            hooks.pre_tool_use("run", &json!({})).await,
            PreToolOutcome::Continue
        );
    }

    #[tokio::test]
    async fn post_hook_feedback() {
        let hooks = Hooks::new(
            &[cfg(
                HookEvent::PostToolUse,
                None,
                "echo lint failed 1>&2 && exit 2",
            )],
            None,
        )
        .unwrap();
        assert_eq!(
            hooks
                .post_tool_use("write", &json!({}), "ok")
                .await
                .as_deref(),
            Some("lint failed")
        );
    }

    #[test]
    fn bad_matcher_is_error() {
        assert!(Hooks::new(&[cfg(HookEvent::PreToolUse, Some("("), "true")], None).is_err());
    }
}
