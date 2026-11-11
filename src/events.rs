//! Shared event types for TUI and headless/RPC parity.
//!
//! Per DESIGN.md §7, every visible TUI state has a 1:1 JSON event.
//! These structs are serialized for headless/RPC and consumed by the TUI renderer.

use serde::{Deserialize, Serialize};

/// A message in the conversation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Message {
    pub role: MessageRole,
    pub content: String,
    pub tool_calls: Option<Vec<ToolCall>>,
    pub tool_call_id: Option<String>,
    pub ts: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum MessageRole {
    User,
    Assistant,
    System,
    Tool,
}

impl std::fmt::Display for MessageRole {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MessageRole::User => write!(f, "user"),
            MessageRole::Assistant => write!(f, "assistant"),
            MessageRole::System => write!(f, "system"),
            MessageRole::Tool => write!(f, "tool"),
        }
    }
}

/// A token delta from streaming model output.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Token {
    pub delta: String,
    pub ts: u64,
}

/// A tool call invocation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub args: serde_json::Value,
    pub status: ToolCallStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ToolCallStatus {
    Pending,
    Ok,
    Error,
}

/// A tool call result.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolResult {
    pub id: String,
    pub name: String,
    pub args: serde_json::Value,
    pub output: String,
    pub ok: bool,
    pub duration_ms: u64,
    pub truncated: bool,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub cancelled: bool,
}

/// An approval request for a privileged operation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ApprovalRequest {
    pub id: String,
    pub tool: String,
    pub args: serde_json::Value,
}

/// An approval decision.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Approval {
    pub id: String,
    pub decision: ApprovalDecision,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub enum ApprovalDecision {
    Allow,
    Deny,
    AllowAlways,
}

/// A todo item.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Todo {
    pub id: String,
    pub title: String,
    pub done: bool,
}

/// Status bar information.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Status {
    pub model: String,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub cost_usd: f64,
    pub elapsed_ms: u64,
    pub retries: u32,
    pub mode: String,
}

/// Git snapshot progress.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SnapshotProgress {
    pub phase: String,
    pub percent: f32,
    pub elapsed_ms: u64,
}

/// A log line.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Log {
    pub level: LogLevel,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

/// An error event.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ErrorEvent {
    pub code: String,
    pub message: String,
}

/// Union of all event types for the event stream.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum Event {
    Message(Message),
    Token(Token),
    ToolCall(ToolCall),
    ToolResult(ToolResult),
    ApprovalRequest(ApprovalRequest),
    Approval(Approval),
    Todo(Todo),
    Status(Status),
    SnapshotProgress(SnapshotProgress),
    Log(Log),
    Error(ErrorEvent),
}

impl Event {
    pub fn message(msg: Message) -> Self {
        Event::Message(msg)
    }

    pub fn token(delta: String, ts: u64) -> Self {
        Event::Token(Token { delta, ts })
    }

    pub fn tool_call(call: ToolCall) -> Self {
        Event::ToolCall(call)
    }

    pub fn tool_result(result: ToolResult) -> Self {
        Event::ToolResult(result)
    }

    pub fn approval_request(req: ApprovalRequest) -> Self {
        Event::ApprovalRequest(req)
    }

    pub fn approval(approval: Approval) -> Self {
        Event::Approval(approval)
    }

    pub fn todo(todo: Todo) -> Self {
        Event::Todo(todo)
    }

    pub fn status(status: Status) -> Self {
        Event::Status(status)
    }

    pub fn snapshot_progress(progress: SnapshotProgress) -> Self {
        Event::SnapshotProgress(progress)
    }

    pub fn log(level: LogLevel, message: String) -> Self {
        Event::Log(Log { level, message })
    }

    pub fn error(code: String, message: String) -> Self {
        Event::Error(ErrorEvent { code, message })
    }
}

/// Broadcast channel for events (TUI + headless share the same stream).
pub type EventSender = tokio::sync::broadcast::Sender<Event>;
pub type EventReceiver = tokio::sync::broadcast::Receiver<Event>;

/// Create a new event broadcast channel with a reasonable buffer.
pub fn event_channel() -> (EventSender, EventReceiver) {
    tokio::sync::broadcast::channel(256)
}
