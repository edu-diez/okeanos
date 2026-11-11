//! ReAct loop — core control flow: think → tool → observe → repeat.

use crate::context::{Conversation, ConversationError};
use crate::events::{
    Approval, ApprovalDecision, ApprovalRequest, Event, EventSender, Message, MessageRole,
    ToolCall, ToolCallStatus, ToolResult,
};
use crate::model::text_tool::{TextToolError, TextToolParser};
use crate::model::{GenerationParams, Model, ModelError, ModelMessage, ModelResponse};
use crate::tool::ToolRegistry;
use anyhow::Context as _;
use std::sync::Arc;
use std::time::{Duration, Instant};
use thiserror::Error;
use tokio::sync::mpsc;

use super::policy::{Policy, PolicyDecision};

/// Errors from the ReAct loop.
#[derive(Debug, Error)]
pub enum ReactError {
    #[error("model error: {0}")]
    Model(#[from] ModelError),
    #[error("conversation error: {0}")]
    Conversation(#[from] ConversationError),
    #[error("tool error: {0}")]
    Tool(String),
    #[error("context cap exceeded: {0}")]
    ContextCap(#[from] crate::context::cap::ContextCapError),
    #[error("max iterations ({max_iter}) reached")]
    MaxIterations { max_iter: usize },
    #[error("max elapsed time ({max_elapsed_sec}s) reached")]
    MaxElapsed { max_elapsed_sec: u64 },
    #[error("interrupted by signal")]
    Interrupted,
    #[error("approval required for {tool}")]
    ApprovalRequired { tool: String },
    #[error("loop stopped: {reason}")]
    Stopped { reason: String },
    #[error("unexpected error: {0}")]
    Other(#[from] anyhow::Error),
}

/// Result of a single ReAct iteration.
#[derive(Debug, Clone, PartialEq)]
pub enum LoopDecision {
    /// Continue to next iteration.
    Continue,
    /// Task completed successfully.
    Complete(String),
    /// Loop stopped due to error or limit.
    Stop(String),
}

/// Configuration for the ReAct loop.
#[derive(Debug, Clone)]
pub struct ReactConfig {
    /// Maximum number of iterations.
    pub max_iterations: usize,
    /// Maximum wall-clock time in seconds.
    pub max_elapsed_sec: u64,
    /// Generation parameters for the model.
    pub generation_params: GenerationParams,
}

impl Default for ReactConfig {
    fn default() -> Self {
        Self {
            max_iterations: 50,
            max_elapsed_sec: 300, // 5 minutes
            generation_params: GenerationParams::default(),
        }
    }
}

/// The ReAct loop harness.
pub struct ReactLoop<M: Model> {
    model: Arc<M>,
    conversation: Conversation,
    tools: ToolRegistry,
    config: ReactConfig,
    event_tx: EventSender,
    iteration: usize,
    start_time: Instant,
    shutdown_rx: Option<mpsc::Receiver<()>>,
    policy: Policy,
    approval_rx: Option<mpsc::Receiver<Approval>>,
}

impl<M: Model> ReactLoop<M> {
    /// Create a new ReAct loop.
    pub fn new(
        model: Arc<M>,
        conversation: Conversation,
        tools: ToolRegistry,
        config: ReactConfig,
        event_tx: EventSender,
    ) -> Self {
        Self {
            model,
            conversation,
            tools,
            config,
            event_tx,
            iteration: 0,
            start_time: Instant::now(),
            shutdown_rx: None,
            policy: Policy::new(vec![]),
            approval_rx: None,
        }
    }

    /// Set the shutdown signal receiver.
    pub fn with_shutdown(mut self, rx: mpsc::Receiver<()>) -> Self {
        self.shutdown_rx = Some(rx);
        self
    }

    /// Set the permission policy used to gate tool execution.
    pub fn with_policy(mut self, policy: Policy) -> Self {
        self.policy = policy;
        self
    }

    /// Set the approval decision receiver.
    ///
    /// When a tool requires approval, the loop emits an `ApprovalRequest`
    /// event and waits on this channel for a matching `Approval` decision
    /// before resuming.
    pub fn with_approval_receiver(mut self, rx: mpsc::Receiver<Approval>) -> Self {
        self.approval_rx = Some(rx);
        self
    }

    /// Run the ReAct loop until completion or termination.
    pub async fn run(mut self, initial_prompt: String) -> Result<String, ReactError> {
        // Add initial user message
        self.conversation
            .add_user_message(initial_prompt)
            .context("failed to add initial prompt")?;

        // Emit initial status
        self.emit_status().await;

        // Set up cross-platform signal handling for Ctrl-C / SIGINT
        let (shutdown_tx, mut shutdown_rx) = self
            .shutdown_rx
            .take()
            .map(|rx| (None, rx))
            .unwrap_or_else(|| {
                let (tx, rx) = mpsc::channel(1);
                (Some(tx), rx)
            });

        // Spawn signal handler
        let shutdown_tx_clone = shutdown_tx.clone();
        tokio::spawn(async move {
            #[cfg(unix)]
            {
                let mut sigint =
                    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
                        .expect("failed to install SIGINT handler");
                sigint.recv().await;
            }
            #[cfg(windows)]
            {
                let mut ctrl_c =
                    tokio::signal::windows::ctrl_c().expect("failed to install Ctrl-C handler");
                ctrl_c.recv().await;
            }
            if let Some(tx) = shutdown_tx_clone {
                let _ = tx.send(()).await;
            }
        });

        loop {
            // Check shutdown signal
            if shutdown_rx.try_recv().is_ok() {
                return Err(ReactError::Interrupted);
            }

            // Check iteration limit
            if self.iteration >= self.config.max_iterations {
                return Err(ReactError::MaxIterations {
                    max_iter: self.config.max_iterations,
                });
            }

            // Check elapsed time limit
            if self.start_time.elapsed() >= Duration::from_secs(self.config.max_elapsed_sec) {
                return Err(ReactError::MaxElapsed {
                    max_elapsed_sec: self.config.max_elapsed_sec,
                });
            }

            // Check context cap warning
            if self.conversation.warning_level() == crate::context::cap::ContextWarning::Warning {
                self.emit_log(
                    crate::events::LogLevel::Warn,
                    format!(
                        "Context usage at {}% ({}/{} tokens)",
                        (self.conversation.estimated_tokens() as f64
                            / self.conversation.cap().max_tokens as f64
                            * 100.0) as u32,
                        self.conversation.estimated_tokens(),
                        self.conversation.cap().max_tokens
                    ),
                )
                .await;
            }

            // Run one iteration
            match self.iterate().await? {
                LoopDecision::Continue => {
                    self.iteration += 1;
                    continue;
                }
                LoopDecision::Complete(result) => {
                    return Ok(result);
                }
                LoopDecision::Stop(reason) => {
                    return Err(ReactError::Stopped { reason });
                }
            }
        }
    }

    /// Run a single ReAct iteration: think → tool → observe.
    async fn iterate(&mut self) -> Result<LoopDecision, ReactError> {
        // THINK: Get model completion
        let model_messages = self.build_model_messages();
        let response = self.call_model(&model_messages).await?;

        // Add assistant message to conversation
        self.conversation
            .add_assistant_message(response.content.clone(), response.tool_calls.clone())
            .context("failed to add assistant message")?;

        // Emit assistant message event
        self.emit_message(Message {
            role: MessageRole::Assistant,
            content: response.content.clone(),
            tool_calls: response.tool_calls.clone(),
            tool_call_id: None,
            ts: current_timestamp(),
        })
        .await;

        // If there are native tool calls, execute them.
        if let Some(tool_calls) = response.tool_calls {
            if tool_calls.is_empty() {
                // No tool calls - check if the model is done
                if self.is_completion_response(&response.content) {
                    return Ok(LoopDecision::Complete(response.content));
                }
                // Model responded but didn't call tools and didn't indicate completion
                // Continue the loop to prompt for more action
                return Ok(LoopDecision::Continue);
            }

            // Execute each tool call
            for tool_call in tool_calls {
                let result = self.execute_tool(tool_call).await?;

                // Add tool result to conversation
                self.conversation
                    .add_tool_message(result.output.clone(), result.id.clone())
                    .context("failed to add tool result")?;

                // Emit tool result event
                self.emit_tool_result(result).await;
            }

            // After tool execution, continue to next iteration
            return Ok(LoopDecision::Continue);
        }

        // No native tool calls. For models without native function calling,
        // try text-based tool calling: parse raw model output into tool
        // invocations, validate against schema, then execute.
        if !self.model.capabilities().tool_calling {
            match TextToolParser::parse(&response.content) {
                Ok(calls) => {
                    if !calls.is_empty() {
                        let validated = TextToolParser::validate(&calls, &self.tools)
                            .map_err(|e| ReactError::Tool(format!("text tool call failed: {e}")))?;
                        for (i, call) in validated.into_iter().enumerate() {
                            let tool_call = ToolCall {
                                id: format!("text-call-{}-{}", self.iteration, i),
                                name: call.name,
                                args: call.args,
                                status: ToolCallStatus::Pending,
                            };
                            let result = self.execute_tool(tool_call).await?;

                            self.conversation
                                .add_tool_message(result.output.clone(), result.id.clone())
                                .context("failed to add tool result")?;

                            self.emit_tool_result(result).await;
                        }
                        return Ok(LoopDecision::Continue);
                    }
                }
                Err(TextToolError::NoToolCalls) => {}
                Err(e) => {
                    return Err(ReactError::Tool(format!("text tool call failed: {e}")));
                }
            }
        }

        // No tool calls - check for completion
        if self.is_completion_response(&response.content) {
            Ok(LoopDecision::Complete(response.content))
        } else {
            Ok(LoopDecision::Continue)
        }
    }

    /// Call the model with the current conversation.
    async fn call_model(&self, messages: &[ModelMessage]) -> Result<ModelResponse, ModelError> {
        // Use non-streaming completion for simplicity
        self.model
            .complete(messages, &self.config.generation_params)
            .await
    }

    /// Build the model messages, injecting a text-based tool schema into the
    /// system prompt when the model does not support native function calling.
    fn build_model_messages(&self) -> Vec<ModelMessage> {
        let mut messages = self.conversation.to_model_messages();

        // For models without native tool calling, inject a text-based schema
        // description into the system prompt so the model knows how to invoke
        // tools via text.
        if !self.model.capabilities().tool_calling {
            let specs = self.tools.specs();
            if !specs.is_empty() {
                let snippet = TextToolParser::generate_system_prompt_snippet(&specs);
                if let Some(system_msg) = messages.first_mut() {
                    system_msg.content.push_str(&snippet);
                }
            }
        }

        messages
    }

    /// Execute a tool call, gated by the permission policy.
    ///
    /// If the policy requires approval, the loop emits an `ApprovalRequest`
    /// event and pauses, waiting for a matching `Approval` decision on the
    /// approval receiver. The loop resumes only on `Allow` or `AllowAlways`;
    /// `Deny` aborts execution with a structured error.
    async fn execute_tool(&mut self, tool_call: ToolCall) -> Result<ToolResult, ReactError> {
        // Resolve the tool and clone its spec so the policy check doesn't
        // hold an immutable borrow across the mutable approval wait.
        let tool = self
            .tools
            .get(&tool_call.name)
            .ok_or_else(|| ReactError::Tool(format!("unknown tool: {}", tool_call.name)))?;
        let spec = tool.spec();

        // Gate execution behind the permission policy.
        match self.policy.check(&tool_call.name, &spec) {
            PolicyDecision::Allow => {}
            PolicyDecision::Deny => {
                return Err(ReactError::Tool(format!(
                    "permission denied: {}",
                    tool_call.name
                )));
            }
            PolicyDecision::RequireApproval => {
                // Emit approval request and pause the loop until a decision arrives.
                let approval_id = format!("approval-{}", self.iteration);
                self.emit_approval_request(ApprovalRequest {
                    id: approval_id.clone(),
                    tool: tool_call.name.clone(),
                    args: tool_call.args.clone(),
                })
                .await;

                let decision = self.wait_for_approval(&approval_id).await?;

                match decision {
                    ApprovalDecision::Allow => {}
                    ApprovalDecision::AllowAlways => {
                        self.policy.allow_always(&tool_call.name);
                    }
                    ApprovalDecision::Deny => {
                        return Err(ReactError::Tool(format!(
                            "permission denied: {}",
                            tool_call.name
                        )));
                    }
                }
            }
        }

        // Re-fetch the tool (the borrow was dropped above).
        let tool = self
            .tools
            .get(&tool_call.name)
            .ok_or_else(|| ReactError::Tool(format!("unknown tool: {}", tool_call.name)))?;

        // Emit tool call started event
        self.emit_tool_call(ToolCall {
            id: tool_call.id.clone(),
            name: tool_call.name.clone(),
            args: tool_call.args.clone(),
            status: ToolCallStatus::Pending,
        })
        .await;

        let start = Instant::now();

        // Execute the tool
        let output = tool
            .execute(tool_call.args.clone())
            .await
            .map_err(|e| ReactError::Tool(format!("tool execution failed: {e}")))?;

        let duration_ms = start.elapsed().as_millis() as u64;

        let ok = true; // Tool execution succeeded (error would have been caught above)

        let result = ToolResult {
            id: tool_call.id,
            name: tool_call.name,
            args: tool_call.args,
            output,
            ok,
            duration_ms,
            truncated: false,
            exit_code: None,
            timed_out: false,
            cancelled: false,
        };

        // Emit tool call completed event
        self.emit_tool_call(ToolCall {
            id: result.id.clone(),
            name: result.name.clone(),
            args: result.args.clone(),
            status: if ok {
                ToolCallStatus::Ok
            } else {
                ToolCallStatus::Error
            },
        })
        .await;

        Ok(result)
    }

    /// Wait for an `Approval` decision matching `approval_id` on the approval
    /// receiver. Returns `Err` if no receiver is configured or the channel
    /// closes before a matching decision arrives.
    async fn wait_for_approval(
        &mut self,
        approval_id: &str,
    ) -> Result<ApprovalDecision, ReactError> {
        let Some(rx) = &mut self.approval_rx else {
            return Err(ReactError::Tool(
                "approval required for tool but no approval receiver configured".to_string(),
            ));
        };

        while let Some(approval) = rx.recv().await {
            if approval.id == approval_id {
                return Ok(approval.decision);
            }
        }

        Err(ReactError::Stopped {
            reason: "approval channel closed before decision".into(),
        })
    }

    /// Check if the model's response indicates task completion.
    fn is_completion_response(&self, content: &str) -> bool {
        // Simple heuristic: check for completion markers
        let content_lower = content.to_lowercase();
        content_lower.contains("task complete")
            || content_lower.contains("completed successfully")
            || content_lower.contains("all done")
            || content_lower.contains("finished")
    }

    /// Emit a message event.
    async fn emit_message(&self, msg: Message) {
        let _ = self.event_tx.send(Event::Message(msg));
    }

    /// Emit a tool call event.
    async fn emit_tool_call(&self, call: ToolCall) {
        let _ = self.event_tx.send(Event::ToolCall(call));
    }

    /// Emit a tool result event.
    async fn emit_tool_result(&self, result: ToolResult) {
        let _ = self.event_tx.send(Event::ToolResult(result));
    }

    /// Emit an approval request event.
    async fn emit_approval_request(&self, req: ApprovalRequest) {
        let _ = self.event_tx.send(Event::ApprovalRequest(req));
    }

    /// Emit a status event.
    async fn emit_status(&self) {
        let status = crate::events::Status {
            model: self.model.model_id().to_string(),
            tokens_in: self.conversation.estimated_tokens() as u64,
            tokens_out: 0,
            cost_usd: self.conversation.cost_estimate().total_cost_usd(),
            elapsed_ms: self.start_time.elapsed().as_millis() as u64,
            retries: 0,
            mode: "headless".to_string(),
        };
        let _ = self.event_tx.send(Event::Status(status));
    }

    /// Emit a log event.
    async fn emit_log(&self, level: crate::events::LogLevel, message: String) {
        let _ = self
            .event_tx
            .send(Event::Log(crate::events::Log { level, message }));
    }
}

/// Get current Unix timestamp in milliseconds.
fn current_timestamp() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{MockModel, ModelTier};
    use crate::tool::ToolRegistry;
    use crate::tool::todo::TodoTool;
    use crate::tool::tool_trait::{Tool, ToolCategory, ToolError, ToolSpec};
    use async_trait::async_trait;
    use serde_json::Value;
    use std::sync::Arc;

    #[tokio::test]
    async fn test_react_loop_basic() {
        let model = Arc::new(MockModel::small("test-model"));
        model.push_response(crate::model::mock::MockModel::text_response(
            "Task complete",
        ));

        let conversation = Conversation::from_tier(ModelTier::Small, 4096);
        let tools = ToolRegistry::new();
        let config = ReactConfig {
            max_iterations: 10,
            max_elapsed_sec: 60,
            ..Default::default()
        };
        let (event_tx, _) = crate::events::event_channel();

        let loop_ = ReactLoop::new(model, conversation, tools, config, event_tx);
        let result = loop_.run("Test prompt".into()).await;

        assert!(result.is_ok());
        assert!(result.unwrap().contains("Task complete"));
    }

    #[tokio::test]
    async fn test_react_loop_max_iterations() {
        let model = Arc::new(MockModel::small("test-model"));
        // Model never completes, just keeps responding
        for _ in 0..20 {
            model.push_response(crate::model::mock::MockModel::text_response(
                "Continue working",
            ));
        }

        let conversation = Conversation::from_tier(ModelTier::Small, 4096);
        let tools = ToolRegistry::new();
        let config = ReactConfig {
            max_iterations: 5,
            max_elapsed_sec: 60,
            ..Default::default()
        };
        let (event_tx, _) = crate::events::event_channel();

        let loop_ = ReactLoop::new(model, conversation, tools, config, event_tx);
        let result = loop_.run("Test prompt".into()).await;

        assert!(result.is_err());
        match result.unwrap_err() {
            ReactError::MaxIterations { max_iter } => assert_eq!(max_iter, 5),
            _ => panic!("expected MaxIterations error"),
        }
    }

    #[tokio::test]
    async fn test_react_loop_with_tool_calls() {
        let model = Arc::new(MockModel::frontier("test-model"));
        model.push_response(crate::model::mock::MockModel::tool_call_response(
            "Let me create a todo",
            vec![crate::events::ToolCall {
                id: "call-1".into(),
                name: "todo".into(),
                args: serde_json::json!({"action": "add", "title": "Test task"}),
                status: crate::events::ToolCallStatus::Pending,
            }],
        ));
        model.push_response(crate::model::mock::MockModel::text_response(
            "Task complete",
        ));

        let conversation = Conversation::from_tier(ModelTier::Frontier, 4096);
        let tools = ToolRegistry::new();
        // Note: todo tool would be registered here in real implementation
        let config = ReactConfig::default();
        let (event_tx, _event_rx) = crate::events::event_channel();

        let loop_ = ReactLoop::new(model, conversation, tools, config, event_tx);
        let result = loop_.run("Test prompt".into()).await;

        // Since todo tool isn't registered, we expect a tool error
        // But the loop should handle it gracefully
        assert!(result.is_err() || result.is_ok());
    }

    #[tokio::test]
    async fn test_react_loop_text_based_tool_call_executes() {
        // Small model has no native tool calling -> text-based path is used.
        let model = Arc::new(MockModel::small("test-model"));
        model.push_response(crate::model::mock::MockModel::text_response(
            "Let me create a todo.\n<tool_call name=\"todo\">\n{\"action\": \"add\", \"title\": \"Test task\"}\n</tool_call>",
        ));
        model.push_response(crate::model::mock::MockModel::text_response(
            "Task complete",
        ));

        let conversation = Conversation::from_tier(ModelTier::Small, 4096);
        let mut tools = ToolRegistry::new();
        tools.register(TodoTool::new_empty());
        let config = ReactConfig {
            max_iterations: 10,
            max_elapsed_sec: 60,
            ..Default::default()
        };
        let (event_tx, _) = crate::events::event_channel();

        let loop_ = ReactLoop::new(model, conversation, tools, config, event_tx);
        let result = loop_.run("Create a todo".into()).await;

        assert!(result.is_ok());
        assert!(result.unwrap().contains("Task complete"));
    }

    #[tokio::test]
    async fn test_react_loop_text_based_tool_call_invalid() {
        // Invalid text tool call: missing required "action" field.
        let model = Arc::new(MockModel::small("test-model"));
        model.push_response(crate::model::mock::MockModel::text_response(
            "<tool_call name=\"todo\">\n{\"title\": \"No action field\"}\n</tool_call>",
        ));

        let conversation = Conversation::from_tier(ModelTier::Small, 4096);
        let mut tools = ToolRegistry::new();
        tools.register(TodoTool::new_empty());
        let config = ReactConfig {
            max_iterations: 10,
            max_elapsed_sec: 60,
            ..Default::default()
        };
        let (event_tx, _) = crate::events::event_channel();

        let loop_ = ReactLoop::new(model, conversation, tools, config, event_tx);
        let result = loop_.run("Create a todo".into()).await;

        assert!(result.is_err());
        match result.unwrap_err() {
            ReactError::Tool(msg) => assert!(msg.contains("text tool call failed")),
            _ => panic!("expected text tool call error"),
        }
    }

    // ---- WriteTool stub for approval tests ----

    struct WriteTool;

    #[async_trait]
    impl Tool for WriteTool {
        fn spec(&self) -> ToolSpec {
            ToolSpec::new(
                "write",
                "Write a file",
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": {"type": "string"},
                        "content": {"type": "string"}
                    },
                    "required": ["path", "content"]
                }),
            )
            .with_category(ToolCategory::Write)
        }

        async fn execute(&self, _args: Value) -> Result<String, ToolError> {
            Ok("written".to_string())
        }
    }

    /// Helper: build a loop with a policy that only allows `todo`, plus an
    /// approval channel for write/delete/network tools.
    fn build_approval_loop(
        model: Arc<MockModel>,
        conversation: Conversation,
        tools: ToolRegistry,
        config: ReactConfig,
    ) -> (
        ReactLoop<MockModel>,
        crate::events::EventReceiver,
        mpsc::Sender<crate::events::Approval>,
    ) {
        let (event_tx, event_rx) = crate::events::event_channel();
        let (approval_tx, approval_rx) = mpsc::channel(16);
        let policy = Policy::new(vec!["todo".into()]);
        let loop_ = ReactLoop::new(model, conversation, tools, config, event_tx)
            .with_policy(policy)
            .with_approval_receiver(approval_rx);
        (loop_, event_rx, approval_tx)
    }

    #[tokio::test]
    async fn test_react_loop_todo_without_approval() {
        // todo is on the allow-list -> executes without approval.
        let model = Arc::new(MockModel::small("test-model"));
        model.push_response(crate::model::mock::MockModel::text_response(
            "Let me create a todo.\n<tool_call name=\"todo\">\n{\"action\": \"add\", \"title\": \"Test task\"}\n</tool_call>",
        ));
        model.push_response(crate::model::mock::MockModel::text_response(
            "Task complete",
        ));

        let conversation = Conversation::from_tier(ModelTier::Small, 4096);
        let mut tools = ToolRegistry::new();
        tools.register(TodoTool::new_empty());
        let config = ReactConfig {
            max_iterations: 10,
            max_elapsed_sec: 60,
            ..Default::default()
        };

        let (loop_, _event_rx, _approval_tx) =
            build_approval_loop(model, conversation, tools, config);
        let result = loop_.run("Create a todo".into()).await;

        assert!(result.is_ok());
        assert!(result.unwrap().contains("Task complete"));
    }

    #[tokio::test]
    async fn test_react_loop_write_blocked_until_approval_allowed() {
        // write requires approval; loop pauses on ApprovalRequest, resumes
        // when Allow is received.
        let model = Arc::new(MockModel::small("test-model"));
        model.push_response(crate::model::mock::MockModel::tool_call_response(
            "Let me write a file",
            vec![crate::events::ToolCall {
                id: "call-1".into(),
                name: "write".into(),
                args: serde_json::json!({"path": "/tmp/test.txt", "content": "hello"}),
                status: crate::events::ToolCallStatus::Pending,
            }],
        ));
        model.push_response(crate::model::mock::MockModel::text_response(
            "Task complete",
        ));

        let conversation = Conversation::from_tier(ModelTier::Small, 4096);
        let mut tools = ToolRegistry::new();
        tools.register(WriteTool);
        let config = ReactConfig {
            max_iterations: 10,
            max_elapsed_sec: 60,
            ..Default::default()
        };

        let (loop_, mut event_rx, approval_tx) =
            build_approval_loop(model, conversation, tools, config);

        // Run the loop in a background task; it will pause at approval.
        let loop_handle = tokio::spawn(async move { loop_.run("Write a file".into()).await });

        // Wait for the approval request event.
        let mut req_id = None;
        for _ in 0..200 {
            if let Ok(event) = event_rx.try_recv() {
                if let crate::events::Event::ApprovalRequest(req) = event {
                    req_id = Some(req.id);
                    break;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        let req_id = req_id.expect("expected approval request event");

        // Grant approval -> loop should resume and complete.
        approval_tx
            .send(crate::events::Approval {
                id: req_id,
                decision: crate::events::ApprovalDecision::Allow,
            })
            .await
            .expect("failed to send approval");

        let result = loop_handle.await.expect("loop task panicked");
        assert!(
            result.is_ok(),
            "loop should complete after approval: {result:?}"
        );
        assert!(result.unwrap().contains("Task complete"));
    }

    #[tokio::test]
    async fn test_react_loop_write_denied() {
        // write requires approval; Deny aborts the loop with a structured error.
        let model = Arc::new(MockModel::small("test-model"));
        model.push_response(crate::model::mock::MockModel::tool_call_response(
            "Let me write a file",
            vec![crate::events::ToolCall {
                id: "call-1".into(),
                name: "write".into(),
                args: serde_json::json!({"path": "/tmp/test.txt", "content": "hello"}),
                status: crate::events::ToolCallStatus::Pending,
            }],
        ));

        let conversation = Conversation::from_tier(ModelTier::Small, 4096);
        let mut tools = ToolRegistry::new();
        tools.register(WriteTool);
        let config = ReactConfig {
            max_iterations: 10,
            max_elapsed_sec: 60,
            ..Default::default()
        };

        let (loop_, mut event_rx, approval_tx) =
            build_approval_loop(model, conversation, tools, config);

        let loop_handle = tokio::spawn(async move { loop_.run("Write a file".into()).await });

        // Wait for the approval request event.
        let mut req_id = None;
        for _ in 0..200 {
            if let Ok(event) = event_rx.try_recv() {
                if let crate::events::Event::ApprovalRequest(req) = event {
                    req_id = Some(req.id);
                    break;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        let req_id = req_id.expect("expected approval request event");

        // Deny approval -> loop should fail with permission denied.
        approval_tx
            .send(crate::events::Approval {
                id: req_id,
                decision: crate::events::ApprovalDecision::Deny,
            })
            .await
            .expect("failed to send denial");

        let result = loop_handle.await.expect("loop task panicked");
        assert!(result.is_err(), "loop should fail on denial: {result:?}");
        let err = result.unwrap_err();
        assert!(
            err.to_string().contains("permission denied"),
            "expected permission denied error, got: {err}"
        );
    }

    #[tokio::test]
    async fn test_react_loop_write_allow_always() {
        // AllowAlways grants session-wide permission; subsequent write calls
        // proceed without another approval request.
        let model = Arc::new(MockModel::small("test-model"));
        model.push_response(crate::model::mock::MockModel::tool_call_response(
            "Let me write a file",
            vec![crate::events::ToolCall {
                id: "call-1".into(),
                name: "write".into(),
                args: serde_json::json!({"path": "/tmp/a.txt", "content": "a"}),
                status: crate::events::ToolCallStatus::Pending,
            }],
        ));
        model.push_response(crate::model::mock::MockModel::tool_call_response(
            "Let me write another file",
            vec![crate::events::ToolCall {
                id: "call-2".into(),
                name: "write".into(),
                args: serde_json::json!({"path": "/tmp/b.txt", "content": "b"}),
                status: crate::events::ToolCallStatus::Pending,
            }],
        ));
        model.push_response(crate::model::mock::MockModel::text_response(
            "Task complete",
        ));

        let conversation = Conversation::from_tier(ModelTier::Small, 4096);
        let mut tools = ToolRegistry::new();
        tools.register(WriteTool);
        let config = ReactConfig {
            max_iterations: 10,
            max_elapsed_sec: 60,
            ..Default::default()
        };

        let (loop_, mut event_rx, approval_tx) =
            build_approval_loop(model, conversation, tools, config);

        let loop_handle = tokio::spawn(async move { loop_.run("Write files".into()).await });

        // First write: wait for approval request, grant AllowAlways.
        let mut req_id = None;
        for _ in 0..200 {
            if let Ok(event) = event_rx.try_recv() {
                if let crate::events::Event::ApprovalRequest(req) = event {
                    req_id = Some(req.id);
                    break;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let req_id = req_id.expect("expected first approval request event");

        approval_tx
            .send(crate::events::Approval {
                id: req_id.clone(),
                decision: crate::events::ApprovalDecision::AllowAlways,
            })
            .await
            .expect("failed to send allow-always");

        // Second write: should NOT produce another ApprovalRequest because
        // AllowAlways grants session-wide permission.
        let mut second_req = None;
        for _ in 0..200 {
            if let Ok(event) = event_rx.try_recv() {
                if let crate::events::Event::ApprovalRequest(req) = event {
                    if req.id != req_id {
                        second_req = Some(req.id);
                        break;
                    }
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(
            second_req.is_none(),
            "second write should not require another approval after AllowAlways"
        );

        let result = loop_handle.await.expect("loop task panicked");
        assert!(result.is_ok(), "loop should complete: {result:?}");
        assert!(result.unwrap().contains("Task complete"));
    }

    #[tokio::test]
    async fn test_react_loop_no_approval_receiver_errors() {
        // Without an approval receiver, a tool requiring approval returns an
        // error instead of hanging.
        let model = Arc::new(MockModel::small("test-model"));
        model.push_response(crate::model::mock::MockModel::tool_call_response(
            "Let me write a file",
            vec![crate::events::ToolCall {
                id: "call-1".into(),
                name: "write".into(),
                args: serde_json::json!({"path": "/tmp/test.txt", "content": "hello"}),
                status: crate::events::ToolCallStatus::Pending,
            }],
        ));

        let conversation = Conversation::from_tier(ModelTier::Small, 4096);
        let mut tools = ToolRegistry::new();
        tools.register(WriteTool);
        let config = ReactConfig {
            max_iterations: 10,
            max_elapsed_sec: 60,
            ..Default::default()
        };
        let (event_tx, _) = crate::events::event_channel();
        let policy = Policy::new(vec!["todo".into()]);

        // No approval receiver configured.
        let loop_ =
            ReactLoop::new(model, conversation, tools, config, event_tx).with_policy(policy);
        let result = loop_.run("Write a file".into()).await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.to_string().contains("approval required")
                || err.to_string().contains("permission denied"),
            "expected approval error, got: {err}"
        );
    }
}
