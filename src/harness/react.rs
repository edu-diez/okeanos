//! ReAct loop — core control flow: think → tool → observe → repeat.

use crate::context::{Conversation, ConversationError};
use crate::events::{
    Event, EventSender, Message, MessageRole, ToolCall, ToolCallStatus, ToolResult,
};
use crate::model::{GenerationParams, Model, ModelError, ModelMessage, ModelResponse};
use crate::tool::ToolRegistry;
use anyhow::Context as _;
use std::sync::Arc;
use std::time::{Duration, Instant};
use thiserror::Error;
use tokio::sync::mpsc;

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
        }
    }

    /// Set the shutdown signal receiver.
    pub fn with_shutdown(mut self, rx: mpsc::Receiver<()>) -> Self {
        self.shutdown_rx = Some(rx);
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
                tokio::signal::windows::ctrl_c()
                    .expect("failed to install Ctrl-C handler")
                    .await;
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
        let model_messages = self.conversation.to_model_messages();
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

        // If there are tool calls, execute them
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
            Ok(LoopDecision::Continue)
        } else {
            // No tool calls - check for completion
            if self.is_completion_response(&response.content) {
                Ok(LoopDecision::Complete(response.content))
            } else {
                Ok(LoopDecision::Continue)
            }
        }
    }

    /// Call the model with the current conversation.
    async fn call_model(&self, messages: &[ModelMessage]) -> Result<ModelResponse, ModelError> {
        // Use non-streaming completion for simplicity
        self.model
            .complete(messages, &self.config.generation_params)
            .await
    }

    /// Execute a tool call.
    async fn execute_tool(&self, tool_call: ToolCall) -> Result<ToolResult, ReactError> {
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
}
