use crate::events::{Event, EventSender};
use crate::session::SessionManager;
use std::sync::Arc;
use tokio::sync::{Mutex, mpsc};

/// Configuration for headless/RPC mode.
#[derive(Debug, Clone)]
pub struct HeadlessConfig {
    /// Approval policy for privileged operations.
    pub approval_policy: ApprovalPolicy,
    /// Timeout for approval prompts (when policy is Prompt).
    pub approval_timeout_sec: u64,
    /// Whether to emit events as JSON to stdout (headless mode).
    pub json_output: bool,
    /// Whether to run in RPC mode (JSON-RPC over stdio).
    pub rpc_mode: bool,
}

/// Approval policy for headless mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalPolicy {
    /// Allow all privileged operations without prompting.
    Auto,
    /// Deny all privileged operations.
    Deny,
    /// Emit ApprovalRequest event and wait for Approval on stdin.
    Prompt,
}

impl Default for HeadlessConfig {
    fn default() -> Self {
        Self {
            approval_policy: ApprovalPolicy::Prompt,
            approval_timeout_sec: 30,
            json_output: true,
            rpc_mode: false,
        }
    }
}

/// Shared event emitter used by both TUI and headless modes.
/// This eliminates duplicate event logging logic.
#[derive(Clone)]
pub struct EventEmitter {
    event_tx: EventSender,
    session_manager: Option<Arc<Mutex<SessionManager>>>,
    json_output: bool,
    rpc_mode: bool,
}

impl EventEmitter {
    /// Create a new event emitter.
    pub fn new(event_tx: EventSender, json_output: bool, rpc_mode: bool) -> Self {
        Self {
            event_tx,
            session_manager: None,
            json_output,
            rpc_mode,
        }
    }

    /// Set the session manager for logging events to session log.
    pub fn with_session(mut self, session: Arc<Mutex<SessionManager>>) -> Self {
        self.session_manager = Some(session);
        self
    }

    /// Emit an event to the event stream.
    pub async fn emit(&self, event: Event) {
        let _ = self.event_tx.send(event.clone());

        // Log to session manager if available
        if let Some(session) = &self.session_manager {
            let mut session = session.lock().await;
            let _ = session.log_event(event.clone());
        }

        // Print JSON to stdout in headless mode
        if self.json_output
            && !self.rpc_mode
            && let Ok(json) = serde_json::to_string(&event)
        {
            println!("{json}");
        }
    }

    /// Emit a message event.
    pub async fn emit_message(&self, msg: crate::events::Message) {
        self.emit(Event::Message(msg)).await;
    }

    /// Emit a token delta event.
    pub async fn emit_token(&self, delta: String, ts: u64) {
        self.emit(Event::Token(crate::events::Token { delta, ts }))
            .await;
    }

    /// Emit a tool call event.
    pub async fn emit_tool_call(&self, call: crate::events::ToolCall) {
        self.emit(Event::ToolCall(call)).await;
    }

    /// Emit a tool result event.
    pub async fn emit_tool_result(&self, result: crate::events::ToolResult) {
        self.emit(Event::ToolResult(result)).await;
    }

    /// Emit an approval request event.
    pub async fn emit_approval_request(&self, req: crate::events::ApprovalRequest) {
        self.emit(Event::ApprovalRequest(req)).await;
    }

    /// Emit an approval decision event.
    pub async fn emit_approval(&self, approval: crate::events::Approval) {
        self.emit(Event::Approval(approval)).await;
    }

    /// Emit a todo event.
    pub async fn emit_todo(&self, todo: crate::events::Todo) {
        self.emit(Event::Todo(todo)).await;
    }

    /// Emit a status event.
    pub async fn emit_status(&self, status: crate::events::Status) {
        self.emit(Event::Status(status)).await;
    }

    /// Emit a snapshot progress event.
    pub async fn emit_snapshot_progress(&self, progress: crate::events::SnapshotProgress) {
        self.emit(Event::SnapshotProgress(progress)).await;
    }

    /// Emit a log event.
    pub async fn emit_log(&self, level: crate::events::LogLevel, message: String) {
        self.emit(Event::Log(crate::events::Log { level, message }))
            .await;
    }

    /// Emit an error event.
    pub async fn emit_error(&self, code: String, message: String) {
        self.emit(Event::Error(crate::events::ErrorEvent { code, message }))
            .await;
    }

    /// Emit a config modal event.
    pub async fn emit_config_modal(
        &self,
        action: crate::events::ConfigModalAction,
        step: Option<u8>,
        data: Option<serde_json::Value>,
    ) {
        self.emit(Event::ConfigModal(crate::events::ConfigModalEvent {
            action,
            step,
            data,
        }))
        .await;
    }

    /// Emit a config saved event.
    pub async fn emit_config_saved(
        &self,
        path: String,
        provider_kind: String,
        model: String,
        base_url: String,
    ) {
        self.emit(Event::ConfigSaved(crate::events::ConfigSaved {
            path,
            provider_kind,
            model,
            base_url,
        }))
        .await;
    }

    /// Emit an interrupted event.
    pub async fn emit_interrupted(&self, user_initiated: bool) {
        self.emit(Event::Interrupted(crate::events::Interrupted {
            user_initiated,
        }))
        .await;
    }
}

/// Handle approval requests in headless mode based on policy.
pub async fn handle_headless_approvals(
    mut approval_rx: mpsc::Receiver<crate::events::ApprovalRequest>,
    approval_tx: mpsc::Sender<crate::events::Approval>,
    config: HeadlessConfig,
    event_emitter: EventEmitter,
) {
    use std::io::{BufRead, BufReader, stdin};
    use tokio::task;
    use tokio::time::{Duration, timeout};

    match config.approval_policy {
        ApprovalPolicy::Auto => {
            // Auto-approve all requests
            while let Some(req) = approval_rx.recv().await {
                let approval = crate::events::Approval {
                    id: req.id,
                    decision: crate::events::ApprovalDecision::Allow,
                };
                let _ = approval_tx.send(approval).await;
                event_emitter
                    .emit_log(
                        crate::events::LogLevel::Info,
                        format!("Auto-approved: {}", req.tool),
                    )
                    .await;
            }
        }
        ApprovalPolicy::Deny => {
            // Deny all requests
            while let Some(req) = approval_rx.recv().await {
                let approval = crate::events::Approval {
                    id: req.id,
                    decision: crate::events::ApprovalDecision::Deny,
                };
                let _ = approval_tx.send(approval).await;
                event_emitter
                    .emit_log(
                        crate::events::LogLevel::Warn,
                        format!("Auto-denied: {}", req.tool),
                    )
                    .await;
            }
        }
        ApprovalPolicy::Prompt => {
            // Emit approval request and wait for stdin JSON
            let mut line = String::new();

            loop {
                // Wait for an approval request
                let req = match approval_rx.recv().await {
                    Some(req) => req,
                    None => break, // Channel closed
                };

                // Emit the approval request event
                event_emitter.emit_approval_request(req.clone()).await;

                // Wait for approval decision on stdin with timeout
                line.clear();
                let read_result =
                    timeout(Duration::from_secs(config.approval_timeout_sec), async {
                        task::spawn_blocking(move || {
                            let mut reader = BufReader::new(stdin());
                            let mut line = String::new();
                            reader.read_line(&mut line).ok()?;
                            Some(line)
                        })
                        .await
                        .ok()
                        .flatten()
                    })
                    .await;

                let approval = match read_result {
                    Ok(Some(line)) if !line.trim().is_empty() => {
                        match serde_json::from_str::<crate::events::Approval>(&line) {
                            Ok(a) if a.id == req.id => a,
                            _ => {
                                event_emitter
                                    .emit_log(
                                        crate::events::LogLevel::Error,
                                        "Invalid approval decision; defaulting to deny".to_string(),
                                    )
                                    .await;
                                crate::events::Approval {
                                    id: req.id,
                                    decision: crate::events::ApprovalDecision::Deny,
                                }
                            }
                        }
                    }
                    Ok(Some(_)) | Ok(None) | Err(_) => {
                        event_emitter
                            .emit_log(
                                crate::events::LogLevel::Warn,
                                format!(
                                    "Approval timeout ({}s); defaulting to deny",
                                    config.approval_timeout_sec
                                ),
                            )
                            .await;
                        crate::events::Approval {
                            id: req.id,
                            decision: crate::events::ApprovalDecision::Deny,
                        }
                    }
                };

                let _ = approval_tx.send(approval).await;
            }
        }
    }
}
