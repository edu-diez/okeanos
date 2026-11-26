//! Lifecycle hooks: user commands run around tool calls and the agent loop.
//!
//! Configured with `[[hooks]]` tables:
//!
//! ```toml
//! [[hooks]]
//! event = "PreToolUse"        # see events below
//! matcher = "^(write|edit)$"  # optional regex on the tool name (tool events only)
//! command = "./scripts/check.sh"
//! timeout_secs = 10           # optional, default 10
//! fail_closed = false         # optional: block when the hook breaks or times out
//! ```
//!
//! Events: `PreToolUse`, `PostToolUse`, `UserPromptSubmit`, `Stop`,
//! `SessionStart`, `SubagentStop`. The hook receives a JSON object on stdin
//! (`event`, `cwd`, plus event data such as `tool_name`, `tool_input`,
//! `tool_output`, `prompt`, `last_message`).
//!
//! Result, in order of precedence:
//! * exit code `2` blocks (`PreToolUse`, `UserPromptSubmit`), asks the model to
//!   continue (`Stop`) or feeds stderr back (`PostToolUse`);
//! * exit code `0` with a JSON object on stdout:
//!   `{"decision": "allow"|"ask"|"deny", "reason": "...",
//!   "updated_input": {...}, "additional_context": "..."}`. `allow` skips the
//!   approval prompt (it never overrides `deny` or `ask` rules), `ask` forces
//!   one, `deny` blocks, `updated_input` replaces the tool arguments;
//! * exit code `0` with plain text: extra context for `UserPromptSubmit` and
//!   `SessionStart`;
//! * any other outcome is logged and ignored (fail-open), unless the hook sets
//!   `fail_closed`.
//!
//! Hooks of one event run concurrently, so the slowest one bounds the latency.
//! They all see the original input; if several return `updated_input`, the
//! last one in configuration order wins.

use futures::future::join_all;
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
    UserPromptSubmit,
    Stop,
    SessionStart,
    SubagentStop,
}

impl HookEvent {
    fn as_str(self) -> &'static str {
        match self {
            Self::PreToolUse => "PreToolUse",
            Self::PostToolUse => "PostToolUse",
            Self::UserPromptSubmit => "UserPromptSubmit",
            Self::Stop => "Stop",
            Self::SessionStart => "SessionStart",
            Self::SubagentStop => "SubagentStop",
        }
    }

    fn is_tool_event(self) -> bool {
        matches!(self, Self::PreToolUse | Self::PostToolUse)
    }
}

/// One `[[hooks]]` entry.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct HookConfig {
    pub event: HookEvent,
    /// Regex matched against the tool name; absent matches every tool.
    /// Ignored by non-tool events.
    #[serde(default)]
    pub matcher: Option<String>,
    pub command: String,
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    /// Treat a hook that fails to run, times out or exits with an unexpected
    /// code as a block instead of ignoring it.
    #[serde(default)]
    pub fail_closed: bool,
}

/// What a hook says about the pending action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HookDecision {
    #[default]
    Continue,
    /// Skip the approval prompt.
    Allow,
    /// Force an approval prompt.
    Ask,
    Block,
}

/// Combined result of the `PreToolUse` hooks.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PreToolOutcome {
    pub decision: HookDecision,
    /// Block reason (for `Block`).
    pub reason: Option<String>,
    /// Replacement tool arguments.
    pub updated_input: Option<Value>,
}

impl PreToolOutcome {
    pub fn is_block(&self) -> bool {
        self.decision == HookDecision::Block
    }

    pub fn block_reason(&self) -> String {
        self.reason
            .clone()
            .unwrap_or_else(|| "blocked by PreToolUse hook".to_string())
    }
}

/// Combined result of the `UserPromptSubmit` hooks.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PromptOutcome {
    /// Reason the prompt was rejected.
    pub blocked: Option<String>,
    /// Extra context to append to the prompt.
    pub context: Option<String>,
}

#[derive(Debug, Default)]
struct HookVerdict {
    decision: HookDecision,
    reason: Option<String>,
    updated_input: Option<Value>,
    context: Option<String>,
}

#[derive(Clone)]
struct Hook {
    event: HookEvent,
    matcher: Option<Regex>,
    command: String,
    timeout: Duration,
    fail_closed: bool,
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
    stdout: String,
    stderr: String,
}

fn clip(mut text: String) -> String {
    text.truncate(text.floor_char_boundary(MAX_FEEDBACK_BYTES));
    text
}

fn non_empty(s: &str) -> Option<String> {
    let s = s.trim();
    (!s.is_empty()).then(|| s.to_string())
}

/// Interpret a hook's outcome.
fn verdict(hook: &Hook, run: Option<HookRun>) -> HookVerdict {
    let Some(run) = run else {
        return if hook.fail_closed {
            HookVerdict {
                decision: HookDecision::Block,
                reason: Some(format!(
                    "hook `{}` failed to run or timed out (fail_closed)",
                    hook.command
                )),
                ..Default::default()
            }
        } else {
            HookVerdict::default()
        };
    };
    match run.code {
        Some(2) => HookVerdict {
            decision: HookDecision::Block,
            reason: non_empty(&run.stderr),
            ..Default::default()
        },
        Some(0) => parse_stdout(&run.stdout),
        code if hook.fail_closed => HookVerdict {
            decision: HookDecision::Block,
            reason: Some(format!(
                "hook `{}` exited with {} (fail_closed)",
                hook.command,
                code.map_or("a signal".to_string(), |c| c.to_string())
            )),
            ..Default::default()
        },
        _ => HookVerdict::default(),
    }
}

/// Parse a successful hook's stdout: a JSON decision object or plain context.
fn parse_stdout(stdout: &str) -> HookVerdict {
    let text = stdout.trim();
    if text.is_empty() {
        return HookVerdict::default();
    }
    let Some(obj) = text
        .starts_with('{')
        .then(|| serde_json::from_str::<Value>(text).ok())
        .flatten()
        .filter(Value::is_object)
    else {
        return HookVerdict {
            context: Some(clip(text.to_string())),
            ..Default::default()
        };
    };
    let decision = match obj.get("decision").and_then(Value::as_str) {
        Some("allow" | "approve") => HookDecision::Allow,
        Some("ask") => HookDecision::Ask,
        Some("deny" | "block") => HookDecision::Block,
        _ => HookDecision::Continue,
    };
    HookVerdict {
        decision,
        reason: obj
            .get("reason")
            .and_then(Value::as_str)
            .and_then(non_empty),
        updated_input: obj.get("updated_input").filter(|v| v.is_object()).cloned(),
        context: obj
            .get("additional_context")
            .and_then(Value::as_str)
            .and_then(non_empty)
            .map(clip),
    }
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
                fail_closed: c.fail_closed,
            });
        }
        Ok(Self { hooks, cwd })
    }

    pub fn is_empty(&self) -> bool {
        self.hooks.is_empty()
    }

    /// Only the `PreToolUse` / `PostToolUse` hooks. Sub-agents use this: their
    /// tool calls are gated like the parent's, but prompt and lifecycle hooks
    /// belong to the user-facing session.
    pub fn tool_events_only(&self) -> Self {
        Self {
            hooks: self
                .hooks
                .iter()
                .filter(|h| h.event.is_tool_event())
                .cloned()
                .collect(),
            cwd: self.cwd.clone(),
        }
    }

    fn matching<'a>(
        &'a self,
        event: HookEvent,
        tool: Option<&'a str>,
    ) -> impl Iterator<Item = &'a Hook> {
        self.hooks.iter().filter(move |h| {
            h.event == event
                && (!event.is_tool_event()
                    || h.matcher
                        .as_ref()
                        .is_none_or(|m| tool.is_some_and(|t| m.is_match(t))))
        })
    }

    /// Run every matching hook concurrently; verdicts are in config order.
    async fn dispatch(
        &self,
        event: HookEvent,
        tool: Option<&str>,
        mut payload: Value,
    ) -> Vec<HookVerdict> {
        let hooks: Vec<&Hook> = self.matching(event, tool).collect();
        if hooks.is_empty() {
            return Vec::new();
        }
        payload["event"] = json!(event.as_str());
        if let Some(cwd) = &self.cwd {
            payload["cwd"] = json!(cwd.to_string_lossy());
        }
        let body = payload.to_string();
        let runs = hooks.iter().map(|h| self.run(h, &body));
        join_all(runs)
            .await
            .into_iter()
            .zip(hooks)
            .map(|(run, hook)| verdict(hook, run))
            .collect()
    }

    /// Run `PreToolUse` hooks. A block from any hook wins; otherwise `ask`
    /// beats `allow`.
    pub async fn pre_tool_use(&self, tool: &str, input: &Value) -> PreToolOutcome {
        let verdicts = self
            .dispatch(
                HookEvent::PreToolUse,
                Some(tool),
                json!({"tool_name": tool, "tool_input": input}),
            )
            .await;
        let mut out = PreToolOutcome::default();
        for v in verdicts {
            match v.decision {
                HookDecision::Block => {
                    return PreToolOutcome {
                        decision: HookDecision::Block,
                        reason: v.reason,
                        updated_input: None,
                    };
                }
                HookDecision::Ask => out.decision = HookDecision::Ask,
                HookDecision::Allow if out.decision != HookDecision::Ask => {
                    out.decision = HookDecision::Allow;
                }
                _ => {}
            }
            if v.updated_input.is_some() {
                out.updated_input = v.updated_input;
            }
        }
        out
    }

    /// Run `PostToolUse` hooks; returns feedback from hooks that blocked.
    pub async fn post_tool_use(&self, tool: &str, input: &Value, output: &str) -> Option<String> {
        let verdicts = self
            .dispatch(
                HookEvent::PostToolUse,
                Some(tool),
                json!({"tool_name": tool, "tool_input": input, "tool_output": output}),
            )
            .await;
        let feedback: Vec<String> = verdicts
            .into_iter()
            .filter(|v| v.decision == HookDecision::Block)
            .filter_map(|v| v.reason)
            .collect();
        (!feedback.is_empty()).then(|| clip(feedback.join("\n")))
    }

    /// Run `UserPromptSubmit` hooks.
    pub async fn user_prompt_submit(&self, prompt: &str) -> PromptOutcome {
        let verdicts = self
            .dispatch(HookEvent::UserPromptSubmit, None, json!({"prompt": prompt}))
            .await;
        let mut out = PromptOutcome::default();
        let mut context = Vec::new();
        for v in verdicts {
            if v.decision == HookDecision::Block {
                out.blocked = Some(
                    v.reason
                        .unwrap_or_else(|| "blocked by UserPromptSubmit hook".to_string()),
                );
                return out;
            }
            context.extend(v.context);
        }
        out.context = (!context.is_empty()).then(|| clip(context.join("\n")));
        out
    }

    /// Run `SessionStart` hooks; returns extra context for the first prompt.
    pub async fn session_start(&self) -> Option<String> {
        let context: Vec<String> = self
            .dispatch(HookEvent::SessionStart, None, json!({}))
            .await
            .into_iter()
            .filter_map(|v| v.context)
            .collect();
        (!context.is_empty()).then(|| clip(context.join("\n")))
    }

    /// Run `Stop` hooks. Returns a reason when a hook wants the agent to keep
    /// working instead of finishing.
    pub async fn stop(&self, last_message: &str) -> Option<String> {
        let reasons: Vec<String> = self
            .dispatch(HookEvent::Stop, None, json!({"last_message": last_message}))
            .await
            .into_iter()
            .filter(|v| v.decision == HookDecision::Block)
            .map(|v| {
                v.reason
                    .unwrap_or_else(|| "blocked by Stop hook".to_string())
            })
            .collect();
        (!reasons.is_empty()).then(|| clip(reasons.join("\n")))
    }

    /// Run `SubagentStop` hooks (observation only).
    pub async fn subagent_stop(&self, subagent_id: &str, summary: &str) {
        self.dispatch(
            HookEvent::SubagentStop,
            None,
            json!({"subagent_id": subagent_id, "summary": summary}),
        )
        .await;
    }

    /// Run one hook. `None` when it could not be run or timed out.
    async fn run(&self, hook: &Hook, body: &str) -> Option<HookRun> {
        let mut cmd = if cfg!(windows) {
            let mut c = Command::new("cmd");
            c.arg("/C");
            // Pass the command verbatim so quoting behaves as in a terminal.
            #[cfg(windows)]
            c.raw_arg(&hook.command);
            c
        } else {
            let mut c = Command::new("sh");
            c.args(["-c", &hook.command]);
            c
        };
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some(cwd) = &self.cwd {
            cmd.current_dir(cwd);
        }
        let mut child = cmd.spawn().ok()?;
        if let Some(mut stdin) = child.stdin.take() {
            // A hook that ignores stdin may close it early; that is fine.
            let _ = stdin.write_all(body.as_bytes()).await;
        }
        let out = tokio::time::timeout(hook.timeout, child.wait_with_output())
            .await
            .ok()?
            .ok()?;
        Some(HookRun {
            code: out.status.code(),
            stdout: String::from_utf8_lossy(&out.stdout).to_string(),
            stderr: String::from_utf8_lossy(&out.stderr).to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `echo` a JSON document portably (sh strips bare double quotes).
    fn echo_json(json: &str) -> String {
        if cfg!(windows) {
            format!("echo {json}")
        } else {
            format!("echo '{json}'")
        }
    }

    fn cfg(event: HookEvent, matcher: Option<&str>, command: &str) -> HookConfig {
        HookConfig {
            event,
            matcher: matcher.map(String::from),
            command: command.to_string(),
            timeout_secs: Some(5),
            fail_closed: false,
        }
    }

    fn block_reason(out: &PreToolOutcome) -> Option<&str> {
        out.is_block().then(|| out.reason.as_deref().unwrap_or(""))
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
        let out = hooks.pre_tool_use("run", &json!({})).await;
        assert_eq!(block_reason(&out), Some("nope"));
        // Non-matching tool is unaffected.
        let out = hooks.pre_tool_use("read", &json!({})).await;
        assert_eq!(out, PreToolOutcome::default());
    }

    #[tokio::test]
    async fn other_failures_are_ignored_unless_fail_closed() {
        let hooks = Hooks::new(&[cfg(HookEvent::PreToolUse, None, "exit 1")], None).unwrap();
        assert!(!hooks.pre_tool_use("run", &json!({})).await.is_block());

        let mut strict = cfg(HookEvent::PreToolUse, None, "exit 1");
        strict.fail_closed = true;
        let hooks = Hooks::new(&[strict], None).unwrap();
        assert!(hooks.pre_tool_use("run", &json!({})).await.is_block());
    }

    #[tokio::test]
    async fn fail_closed_blocks_on_timeout() {
        let mut slow = cfg(
            HookEvent::PreToolUse,
            None,
            if cfg!(windows) {
                "ping -n 6 127.0.0.1 >nul"
            } else {
                "sleep 5"
            },
        );
        slow.timeout_secs = Some(1);
        slow.fail_closed = true;
        let hooks = Hooks::new(&[slow], None).unwrap();
        assert!(hooks.pre_tool_use("run", &json!({})).await.is_block());
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

    #[tokio::test]
    async fn json_stdout_decides_and_rewrites_input() {
        let cmd = echo_json(r#"{"decision":"allow","updated_input":{"path":"safe.txt"}}"#);
        let hooks = Hooks::new(&[cfg(HookEvent::PreToolUse, None, &cmd)], None).unwrap();
        let out = hooks.pre_tool_use("write", &json!({"path": "x"})).await;
        assert_eq!(out.decision, HookDecision::Allow);
        assert_eq!(out.updated_input, Some(json!({"path": "safe.txt"})));
    }

    #[tokio::test]
    async fn ask_beats_allow_and_block_beats_all() {
        let allow = echo_json(r#"{"decision":"allow"}"#);
        let ask = echo_json(r#"{"decision":"ask"}"#);
        let deny = echo_json(r#"{"decision":"deny","reason":"no"}"#);
        let (allow, ask, deny) = (allow.as_str(), ask.as_str(), deny.as_str());
        let hooks = Hooks::new(
            &[
                cfg(HookEvent::PreToolUse, None, allow),
                cfg(HookEvent::PreToolUse, None, ask),
            ],
            None,
        )
        .unwrap();
        assert_eq!(
            hooks.pre_tool_use("w", &json!({})).await.decision,
            HookDecision::Ask
        );
        let hooks = Hooks::new(
            &[
                cfg(HookEvent::PreToolUse, None, allow),
                cfg(HookEvent::PreToolUse, None, deny),
            ],
            None,
        )
        .unwrap();
        let out = hooks.pre_tool_use("w", &json!({})).await;
        assert_eq!(block_reason(&out), Some("no"));
    }

    #[tokio::test]
    async fn prompt_hook_blocks_or_adds_context() {
        let hooks = Hooks::new(
            &[cfg(HookEvent::UserPromptSubmit, None, "echo branch main")],
            None,
        )
        .unwrap();
        let out = hooks.user_prompt_submit("hi").await;
        assert_eq!(out.context.as_deref(), Some("branch main"));
        assert!(out.blocked.is_none());

        let hooks = Hooks::new(
            &[cfg(
                HookEvent::UserPromptSubmit,
                None,
                "echo no secrets 1>&2 && exit 2",
            )],
            None,
        )
        .unwrap();
        let out = hooks.user_prompt_submit("hi").await;
        assert_eq!(out.blocked.as_deref(), Some("no secrets"));
    }

    #[tokio::test]
    async fn stop_hook_can_request_continuation() {
        let hooks = Hooks::new(
            &[cfg(
                HookEvent::Stop,
                None,
                "echo run the tests 1>&2 && exit 2",
            )],
            None,
        )
        .unwrap();
        assert_eq!(hooks.stop("done").await.as_deref(), Some("run the tests"));
        assert!(Hooks::default().stop("done").await.is_none());
    }

    #[test]
    fn parse_stdout_variants() {
        assert_eq!(parse_stdout("").decision, HookDecision::Continue);
        let v = parse_stdout("plain text");
        assert_eq!(v.context.as_deref(), Some("plain text"));
        let v = parse_stdout(r#"{"decision":"deny","reason":"r","additional_context":"c"}"#);
        assert_eq!(v.decision, HookDecision::Block);
        assert_eq!(v.reason.as_deref(), Some("r"));
        assert_eq!(v.context.as_deref(), Some("c"));
        // Not an object with a recognised shape: treated as text.
        assert_eq!(parse_stdout("{oops").context.as_deref(), Some("{oops"));
    }

    #[test]
    fn bad_matcher_is_error() {
        assert!(Hooks::new(&[cfg(HookEvent::PreToolUse, Some("("), "true")], None).is_err());
    }
}
