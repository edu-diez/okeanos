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
    /// Optional thinking/reasoning content (for frontier models with thinking capability).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<String>,
    /// Reasoning blocks, images and error flag (omitted from logs when empty).
    #[serde(default, skip_serializing_if = "crate::model::MessageExtras::is_empty")]
    pub extras: crate::model::MessageExtras,
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

/// A live output chunk from a running tool (ephemeral; not session-logged).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolOutput {
    /// Id of the tool call this chunk belongs to.
    pub id: String,
    pub name: String,
    /// Chunk sequence number within the call, starting at 0.
    pub seq: u64,
    pub chunk: String,
    pub ts: u64,
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
    pub provider_configured: bool,
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

/// A turn interruption event (model generation cancelled by user).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Interrupted {
    /// Whether the interruption was user-initiated (Esc key) or signal-based (SIGINT).
    pub user_initiated: bool,
}

/// A thinking/reasoning channel event from the model.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Thinking {
    /// The thinking content (reasoning trace).
    pub content: String,
    /// Timestamp in milliseconds.
    pub ts: u64,
}

/// Sub-agent progress event (emitted periodically during sub-agent execution).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SubAgentProgress {
    /// Sub-agent ID.
    pub subagent_id: String,
    /// Current iteration number.
    pub iteration: usize,
    /// Brief status message.
    pub message: String,
    /// Timestamp in milliseconds.
    pub ts: u64,
}

/// Sub-agent completion event.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SubAgentComplete {
    /// Sub-agent ID.
    pub subagent_id: String,
    /// The todo ID that was delegated to this sub-agent.
    pub todo_id: String,
    /// Final result summary.
    pub summary: String,
    /// Tokens used by the sub-agent.
    pub tokens_used: u64,
    /// The final tool result from the sub-agent.
    pub result: ToolResult,
    /// Timestamp in milliseconds.
    pub ts: u64,
}

/// Sub-agent error event.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SubAgentError {
    /// Sub-agent ID.
    pub subagent_id: String,
    /// Error code.
    pub code: String,
    /// Error message.
    pub message: String,
    /// Timestamp in milliseconds.
    pub ts: u64,
}

/// Context compaction event.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CompactionEvent {
    /// The trigger that initiated compaction.
    pub trigger: CompactionTrigger,
    /// The compaction strategy used.
    pub strategy: CompactionStrategy,
    /// Tokens before compaction.
    pub tokens_before: usize,
    /// Tokens after compaction.
    pub tokens_after: usize,
    /// Number of messages removed.
    pub messages_removed: usize,
    /// Number of turns summarized.
    pub turns_summarized: usize,
    /// Whether native provider compaction was used.
    pub native_provider: Option<String>,
    /// Human-readable summary of what was compacted.
    pub summary: String,
    /// Timestamp in milliseconds.
    pub ts: u64,
}

/// Trigger that initiated compaction.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum CompactionTrigger {
    /// Automatic compaction at threshold.
    Auto,
    /// Manual compaction via /compact command.
    Manual,
    /// Compaction due to provider context overflow.
    OverflowRecovery,
    /// Compaction after model switch to smaller context.
    ModelDownshift,
}

/// Compaction strategy used.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum CompactionStrategy {
    /// Deterministic truncation with dropped-work summaries.
    Basic,
    /// LLM-based summarization.
    Agentic,
    /// Native provider compaction (OpenAI Responses, Anthropic).
    Native,
    /// Old tool outputs cleared; nothing was summarized.
    Prune,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ConfigModalAction {
    Open,
    Step,
    Close,
}

/// Payload for `config_modal` events.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ConfigModalEvent {
    pub action: ConfigModalAction,
    pub step: Option<u8>,
    pub data: Option<serde_json::Value>,
}

/// Payload for `config_saved` event.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ConfigSaved {
    pub path: String,
    pub provider_kind: String,
    pub model: String,
    pub base_url: String,
}

/// Union of all event types for the event stream.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum Event {
    Message(Message),
    Token(Token),
    ToolCall(ToolCall),
    ToolResult(ToolResult),
    ToolOutput(ToolOutput),
    ApprovalRequest(ApprovalRequest),
    Approval(Approval),
    Todo(Todo),
    Status(Status),
    SnapshotProgress(SnapshotProgress),
    Log(Log),
    Error(ErrorEvent),
    ConfigModal(ConfigModalEvent),
    ConfigSaved(ConfigSaved),
    Interrupted(Interrupted),
    Thinking(Thinking),
    Compaction(CompactionEvent),
    SubAgentProgress(SubAgentProgress),
    SubAgentComplete(SubAgentComplete),
    SubAgentError(SubAgentError),
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

    pub fn tool_output(output: ToolOutput) -> Self {
        Event::ToolOutput(output)
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

    pub fn config_modal(
        action: ConfigModalAction,
        step: Option<u8>,
        data: Option<serde_json::Value>,
    ) -> Self {
        Event::ConfigModal(ConfigModalEvent { action, step, data })
    }

    pub fn config_saved(
        path: String,
        provider_kind: String,
        model: String,
        base_url: String,
    ) -> Self {
        Event::ConfigSaved(ConfigSaved {
            path,
            provider_kind,
            model,
            base_url,
        })
    }

    pub fn interrupted(user_initiated: bool) -> Self {
        Event::Interrupted(Interrupted { user_initiated })
    }

    pub fn thinking(content: String, ts: u64) -> Self {
        Event::Thinking(Thinking { content, ts })
    }

    pub fn compaction(event: CompactionEvent) -> Self {
        Event::Compaction(event)
    }

    pub fn subagent_progress(event: SubAgentProgress) -> Self {
        Event::SubAgentProgress(event)
    }

    pub fn subagent_complete(event: SubAgentComplete) -> Self {
        Event::SubAgentComplete(event)
    }

    pub fn subagent_error(event: SubAgentError) -> Self {
        Event::SubAgentError(event)
    }
}

/// Broadcast channel for events (TUI + headless share the same stream).
pub type EventSender = tokio::sync::broadcast::Sender<Event>;
pub type EventReceiver = tokio::sync::broadcast::Receiver<Event>;

/// Create a new event broadcast channel with a reasonable buffer.
pub fn event_channel() -> (EventSender, EventReceiver) {
    tokio::sync::broadcast::channel(256)
}
