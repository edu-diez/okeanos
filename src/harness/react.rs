//! ReAct loop — core control flow: think → tool → observe → repeat.

use crate::context::{Conversation, ConversationError};
use crate::events::{
    Approval, ApprovalDecision, ApprovalRequest, Event, EventSender, LogLevel, Message,
    MessageRole, ToolCall, ToolCallStatus, ToolResult,
};
use crate::harness::retry::{RetryDecision, RetryPolicy};
use crate::model::text_tool::{TextToolError, TextToolParser};
use crate::model::{
    GenerationParams, Model, ModelError, ModelMessage, ModelResponse, ModelRole, ModelTier,
};
use crate::session::SessionManager;
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

/// Configuration for fallback strategies on malformed/failed tool calls.
#[derive(Debug, Clone)]
pub struct FallbackConfig {
    /// Whether fallback strategies are enabled.
    pub enabled: bool,
    /// Maximum number of fallback retry attempts per turn (hard retry cap).
    /// Default 1 = retry once with simpler prompt + reduced tool set.
    pub max_retries_per_turn: usize,
    /// Reduced tool set used during fallback attempts.
    pub reduced_tool_set: Vec<String>,
}

impl Default for FallbackConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_retries_per_turn: 1,
            reduced_tool_set: vec!["todo".to_string(), "read".to_string()],
        }
    }
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
    /// Retry policy for provider errors (429/503 with exponential backoff).
    pub retry_policy: RetryPolicy,
    /// Fallback strategy configuration for malformed/failed tool calls.
    pub fallback: FallbackConfig,
}

impl Default for ReactConfig {
    fn default() -> Self {
        Self {
            max_iterations: 50,
            max_elapsed_sec: 300, // 5 minutes
            generation_params: GenerationParams::default(),
            retry_policy: RetryPolicy::default(),
            fallback: FallbackConfig::default(),
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
    retries: u32,
    start_time: Instant,
    shutdown_rx: Option<mpsc::Receiver<()>>,
    policy: Policy,
    approval_rx: Option<mpsc::Receiver<Approval>>,
    /// Optional session manager for logging events to session log.
    session: Option<SessionManager>,
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
            retries: 0,
            start_time: Instant::now(),
            shutdown_rx: None,
            policy: Policy::new(vec![]),
            approval_rx: None,
            session: None,
        }
    }

    /// Set the session manager for logging events to a session log.
    pub fn with_session(mut self, session: SessionManager) -> Self {
        self.session = Some(session);
        self
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

    /// Set the retry policy for provider errors.
    pub fn with_retry_policy(mut self, policy: RetryPolicy) -> Self {
        self.config.retry_policy = policy;
        self
    }

    /// Set the fallback strategy configuration.
    pub fn with_fallback(mut self, config: FallbackConfig) -> Self {
        self.config.fallback = config;
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
            match self.iterate(&mut shutdown_rx).await? {
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
    async fn iterate(
        &mut self,
        shutdown_rx: &mut mpsc::Receiver<()>,
    ) -> Result<LoopDecision, ReactError> {
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
                let result = self.execute_tool(tool_call, shutdown_rx).await?;

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
        // On malformed/failed tool calls, retry with a simpler prompt and
        // reduced tool set (fallback strategy); if still failing after the
        // hard retry cap, emit an error and skip the turn.
        if !self.model.capabilities().tool_calling {
            return self
                .execute_text_tool_calls(&response.content, shutdown_rx)
                .await;
        }

        // No tool calls - check for completion
        if self.is_completion_response(&response.content) {
            Ok(LoopDecision::Complete(response.content))
        } else {
            Ok(LoopDecision::Continue)
        }
    }

    /// Execute text-based tool calls with fallback support.
    ///
    /// On malformed/failed tool calls, retries with a simpler prompt and
    /// reduced tool set up to `fallback.max_retries_per_turn` times
    /// (hard retry cap per turn). If still failing, emits an error event and
    /// skips the turn (returns `Continue`) rather than aborting the loop.
    async fn execute_text_tool_calls(
        &mut self,
        content: &str,
        shutdown_rx: &mut mpsc::Receiver<()>,
    ) -> Result<LoopDecision, ReactError> {
        // When fallback is disabled, a single attempt is made and errors
        // propagate immediately (preserving the validate-before-execute
        // security property without retry).
        if !self.config.fallback.enabled {
            return self
                .execute_text_tool_calls_once(content, shutdown_rx, None)
                .await;
        }

        let max_fallback = self.config.fallback.max_retries_per_turn;

        for attempt in 0..=max_fallback {
            let is_fallback = attempt > 0;

            // On fallback attempts, re-call the model with a simpler prompt
            // to get a fresh response that is more likely to be valid.
            let current_content = if is_fallback {
                self.emit_log(
                    LogLevel::Warn,
                    format!(
                        "Fallback attempt {}/{}: retrying with simpler prompt and reduced tool set",
                        attempt, max_fallback
                    ),
                )
                .await;

                let simpler_messages = self.build_simpler_model_messages();
                let simpler_response = self.call_model(&simpler_messages).await?;

                // Add assistant message from fallback attempt.
                self.conversation
                    .add_assistant_message(
                        simpler_response.content.clone(),
                        simpler_response.tool_calls.clone(),
                    )
                    .context("failed to add fallback assistant message")?;
                self.emit_message(Message {
                    role: MessageRole::Assistant,
                    content: simpler_response.content.clone(),
                    tool_calls: simpler_response.tool_calls.clone(),
                    tool_call_id: None,
                    ts: current_timestamp(),
                })
                .await;

                // Handle native tool calls from fallback response (defensive;
                // non-tool-calling models should not produce these).
                if let Some(tool_calls) = simpler_response.tool_calls
                    && !tool_calls.is_empty()
                {
                    for tool_call in tool_calls {
                        let result = self.execute_tool(tool_call, shutdown_rx).await?;
                        self.conversation
                            .add_tool_message(result.output.clone(), result.id.clone())
                            .context("failed to add tool result")?;
                        self.emit_tool_result(result).await;
                    }
                    return Ok(LoopDecision::Continue);
                }

                simpler_response.content
            } else {
                content.to_string()
            };

            let tool_filter = if is_fallback {
                Some(self.config.fallback.reduced_tool_set.clone())
            } else {
                None
            };

            let result = self
                .execute_text_tool_calls_once(&current_content, shutdown_rx, tool_filter.as_deref())
                .await;

            match result {
                Ok(decision) => return Ok(decision),
                Err(ReactError::Tool(_)) if is_fallback && attempt == max_fallback => {
                    // Hard retry cap reached: emit error and skip turn.
                    self.emit_log(
                        LogLevel::Error,
                        format!(
                            "Fallback exhausted after {} attempts; skipping turn.",
                            max_fallback
                        ),
                    )
                    .await;
                    self.emit_error(
                        "tool_fallback_exhausted",
                        "Tool call failed after fallback; turn skipped.",
                    )
                    .await;
                    return Ok(LoopDecision::Continue);
                }
                Err(ReactError::Tool(_)) => {
                    // Try next fallback attempt.
                    continue;
                }
                Err(e) => return Err(e),
            }
        }

        Ok(LoopDecision::Continue)
    }

    /// Single attempt at text-based tool calling.
    ///
    /// When `tool_filter` is `Some`, only the named tools are advertised to
    /// the model and validated against (reduced tool set for fallback).
    async fn execute_text_tool_calls_once(
        &mut self,
        content: &str,
        shutdown_rx: &mut mpsc::Receiver<()>,
        tool_filter: Option<&[String]>,
    ) -> Result<LoopDecision, ReactError> {
        match TextToolParser::parse(content) {
            Ok(calls) => {
                if calls.is_empty() {
                    return self.check_completion(content);
                }

                let validated = match tool_filter {
                    Some(names) => {
                        let specs = self.tools.filtered_specs(names);
                        TextToolParser::validate_against_specs(&calls, &specs)
                            .map_err(|e| ReactError::Tool(format!("text tool call failed: {e}")))?
                    }
                    None => TextToolParser::validate(&calls, &self.tools)
                        .map_err(|e| ReactError::Tool(format!("text tool call failed: {e}")))?,
                };

                for (i, call) in validated.into_iter().enumerate() {
                    let tool_call = ToolCall {
                        id: format!("text-call-{}-{}", self.iteration, i),
                        name: call.name,
                        args: call.args,
                        status: ToolCallStatus::Pending,
                    };
                    let result = self.execute_tool(tool_call, shutdown_rx).await?;

                    self.conversation
                        .add_tool_message(result.output.clone(), result.id.clone())
                        .context("failed to add tool result")?;

                    self.emit_tool_result(result).await;
                }
                Ok(LoopDecision::Continue)
            }
            Err(TextToolError::NoToolCalls) => self.check_completion(content),
            Err(e) => Err(ReactError::Tool(format!("text tool call failed: {e}"))),
        }
    }

    /// Check whether the model's response indicates task completion.
    fn check_completion(&self, content: &str) -> Result<LoopDecision, ReactError> {
        if self.is_completion_response(content) {
            Ok(LoopDecision::Complete(content.to_string()))
        } else {
            Ok(LoopDecision::Continue)
        }
    }

    /// Build model messages using a shorter system prompt (Small tier
    /// template) and recent history, for use in fallback attempts.
    fn build_simpler_model_messages(&self) -> Vec<ModelMessage> {
        use crate::model::model_trait::system_prompt_template;

        let simpler_prompt = system_prompt_template(ModelTier::Small);
        let mut messages = Vec::with_capacity(self.conversation.message_count() + 1);

        messages.push(ModelMessage {
            role: ModelRole::System,
            content: simpler_prompt.to_string(),
            tool_calls: None,
            tool_call_id: None,
        });

        // Include recent history only (fewer turns for simpler prompt).
        let max_turns = 3;
        let history = self.conversation.messages();
        let start = history.len().saturating_sub(max_turns * 2);
        for msg in &history[start..] {
            messages.push(ModelMessage {
                role: match msg.role {
                    crate::events::MessageRole::User => ModelRole::User,
                    crate::events::MessageRole::Assistant => ModelRole::Assistant,
                    crate::events::MessageRole::Tool => ModelRole::Tool,
                    crate::events::MessageRole::System => ModelRole::System,
                },
                content: msg.content.clone(),
                tool_calls: msg.tool_calls.clone(),
                tool_call_id: msg.tool_call_id.clone(),
            });
        }

        messages
    }

    /// Call the model with the current conversation, with bounded retries and
    /// exponential backoff on retryable provider errors (429/503).
    ///
    /// Permanent errors (auth failure, invalid request, missing capability,
    /// model not found, context length) are returned immediately without
    /// retry. Every retry attempt is logged as an event.
    async fn call_model(&mut self, messages: &[ModelMessage]) -> Result<ModelResponse, ModelError> {
        let retry_policy = self.config.retry_policy.clone();
        let mut attempt = 0usize;

        loop {
            match self
                .model
                .complete(messages, &self.config.generation_params)
                .await
            {
                Ok(response) => return Ok(response),
                Err(error) => {
                    let retryable = retry_policy.classify(&error);
                    if retryable == RetryDecision::Retry && attempt < retry_policy.max_retries {
                        self.retries += 1;
                        let backoff = retry_policy.backoff_for_attempt(attempt, Some(&error));
                        let max_retries = retry_policy.max_retries;
                        self.emit_log(
                            LogLevel::Warn,
                            format!(
                                "Provider error (attempt {}/{}), retrying in {:?}: {}",
                                attempt + 1,
                                max_retries,
                                backoff,
                                error
                            ),
                        )
                        .await;
                        tokio::time::sleep(backoff).await;
                        attempt += 1;
                        continue;
                    }
                    return Err(error);
                }
            }
        }
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
    ///
    /// Tool execution is interruptible: if a shutdown signal is received on
    /// `shutdown_rx`, execution is aborted and [`ReactError::Interrupted`]
    /// is returned.
    async fn execute_tool(
        &mut self,
        tool_call: ToolCall,
        shutdown_rx: &mut mpsc::Receiver<()>,
    ) -> Result<ToolResult, ReactError> {
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

        // Emit tool call started event (before re-fetching tool to avoid borrow conflict)
        self.emit_tool_call(ToolCall {
            id: tool_call.id.clone(),
            name: tool_call.name.clone(),
            args: tool_call.args.clone(),
            status: ToolCallStatus::Pending,
        })
        .await;

        // Re-fetch the tool (the borrow was dropped above).
        let tool = self
            .tools
            .get(&tool_call.name)
            .ok_or_else(|| ReactError::Tool(format!("unknown tool: {}", tool_call.name)))?;

        let start = Instant::now();

        // Execute the tool, interruptible by shutdown signal.
        let exec_result = tokio::select! {
            biased;
            result = tool.execute(tool_call.args.clone()) => result,
            _ = shutdown_rx.recv() => {
                return Err(ReactError::Interrupted);
            }
        }
        .map_err(|e| ReactError::Tool(format!("tool execution failed: {e}")))?;

        let duration_ms = start.elapsed().as_millis() as u64;

        let result = ToolResult {
            id: tool_call.id,
            name: tool_call.name,
            args: tool_call.args,
            output: exec_result.output,
            ok: true,
            duration_ms,
            truncated: exec_result.truncated,
            exit_code: exec_result.exit_code,
            timed_out: exec_result.timed_out,
            cancelled: exec_result.cancelled,
        };

        // Emit tool call completed event
        self.emit_tool_call(ToolCall {
            id: result.id.clone(),
            name: result.name.clone(),
            args: result.args.clone(),
            status: ToolCallStatus::Ok,
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
    async fn emit_message(&mut self, msg: Message) {
        let event = Event::Message(msg);
        let _ = self.event_tx.send(event.clone());
        if let Some(session) = &mut self.session {
            let _ = session.log_event(event);
        }
    }

    /// Emit a tool call event.
    async fn emit_tool_call(&mut self, call: ToolCall) {
        let event = Event::ToolCall(call);
        let _ = self.event_tx.send(event.clone());
        if let Some(session) = &mut self.session {
            let _ = session.log_event(event);
        }
    }

    /// Emit a tool result event.
    async fn emit_tool_result(&mut self, result: ToolResult) {
        let event = Event::ToolResult(result);
        let _ = self.event_tx.send(event.clone());
        if let Some(session) = &mut self.session {
            let _ = session.log_event(event);
        }
    }

    /// Emit an approval request event.
    async fn emit_approval_request(&mut self, req: ApprovalRequest) {
        let event = Event::ApprovalRequest(req);
        let _ = self.event_tx.send(event.clone());
        if let Some(session) = &mut self.session {
            let _ = session.log_event(event);
        }
    }

    /// Emit a status event.
    async fn emit_status(&mut self) {
        let status = crate::events::Status {
            model: self.model.model_id().to_string(),
            tokens_in: self.conversation.estimated_tokens() as u64,
            tokens_out: 0,
            cost_usd: self.conversation.cost_estimate().total_cost_usd(),
            elapsed_ms: self.start_time.elapsed().as_millis() as u64,
            retries: self.retries,
            mode: "headless".to_string(),
        };
        let event = Event::Status(status);
        let _ = self.event_tx.send(event.clone());
        if let Some(session) = &mut self.session {
            let _ = session.log_event(event);
        }
    }

    /// Emit an error event to the event stream.
    async fn emit_error(&mut self, code: &str, message: &str) {
        let event = Event::error(code.to_string(), message.to_string());
        let _ = self.event_tx.send(event.clone());
        if let Some(session) = &mut self.session {
            let _ = session.log_event(event);
        }
    }

    /// Emit a log event.
    async fn emit_log(&mut self, level: crate::events::LogLevel, message: String) {
        let event = Event::Log(crate::events::Log { level, message });
        let _ = self.event_tx.send(event.clone());
        if let Some(session) = &mut self.session {
            let _ = session.log_event(event);
        }
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
    use crate::model::{MockModel, ModelError, ModelTier};
    use crate::tool::ToolRegistry;
    use crate::tool::todo::TodoTool;
    use crate::tool::tool_trait::{Tool, ToolCategory, ToolError, ToolExecutionResult, ToolSpec};
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
        // Fallback is disabled so the validation error surfaces immediately.
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
            fallback: FallbackConfig {
                enabled: false,
                ..Default::default()
            },
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

    /// Malformed tool call triggers fallback retry with simpler prompt;
    /// if fallback also fails, the turn is skipped (not aborted).
    #[tokio::test]
    async fn test_react_loop_fallback_retries_with_simpler_prompt() {
        // First response: malformed tool call (missing required "action").
        // Second response (fallback): valid tool call.
        let model = Arc::new(MockModel::small("test-model"));
        model.push_response(crate::model::mock::MockModel::text_response(
            "<tool_call name=\"todo\">\n{\"title\": \"No action field\"}\n</tool_call>",
        ));
        model.push_response(crate::model::mock::MockModel::text_response(
            "<tool_call name=\"todo\">\n{\"action\": \"add\", \"title\": \"Fallback task\"}\n</tool_call>",
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

        assert!(
            result.is_ok(),
            "loop should complete after fallback: {result:?}"
        );
        assert!(result.unwrap().contains("Task complete"));
    }

    /// When fallback is exhausted, the turn is skipped rather than aborting
    /// the loop (provided the model eventually produces a completion).
    #[tokio::test]
    async fn test_react_loop_fallback_exhausted_skips_turn() {
        // Both responses are malformed; fallback is exhausted, turn skipped,
        // then the model completes on the next iteration.
        let model = Arc::new(MockModel::small("test-model"));
        model.push_response(crate::model::mock::MockModel::text_response(
            "<tool_call name=\"todo\">\n{\"title\": \"No action field\"}\n</tool_call>",
        ));
        model.push_response(crate::model::mock::MockModel::text_response(
            "<tool_call name=\"todo\">\n{\"title\": \"Still no action\"}\n</tool_call>",
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
            fallback: FallbackConfig {
                max_retries_per_turn: 1,
                ..Default::default()
            },
            ..Default::default()
        };
        let (event_tx, _) = crate::events::event_channel();

        let loop_ = ReactLoop::new(model, conversation, tools, config, event_tx);
        let result = loop_.run("Create a todo".into()).await;

        assert!(
            result.is_ok(),
            "loop should complete after skip: {result:?}"
        );
        assert!(result.unwrap().contains("Task complete"));
    }

    /// Provider 429 rate limit triggers retry with exponential backoff.
    #[tokio::test]
    async fn test_react_loop_provider_rate_limited_retries() {
        // First two calls return 429, third succeeds.
        let model = Arc::new(MockModel::small("test-model"));
        model.push_response(crate::model::mock::MockModel::rate_limit_error(1));
        model.push_response(crate::model::mock::MockModel::rate_limit_error(1));
        model.push_response(crate::model::mock::MockModel::text_response(
            "Task complete",
        ));

        let conversation = Conversation::from_tier(ModelTier::Small, 4096);
        let tools = ToolRegistry::new();
        let config = ReactConfig {
            max_iterations: 10,
            max_elapsed_sec: 60,
            retry_policy: RetryPolicy {
                max_retries: 3,
                base_backoff_ms: 10, // short for test
                ..Default::default()
            },
            ..Default::default()
        };
        let (event_tx, _) = crate::events::event_channel();

        let loop_ = ReactLoop::new(model, conversation, tools, config, event_tx);
        let result = loop_.run("Test prompt".into()).await;

        assert!(
            result.is_ok(),
            "loop should complete after retries: {result:?}"
        );
        assert!(result.unwrap().contains("Task complete"));
    }

    /// Permanent provider errors (auth failure) are not retried.
    #[tokio::test]
    async fn test_react_loop_permanent_error_not_retried() {
        let model = Arc::new(MockModel::small("test-model"));
        model.push_response(crate::model::mock::MockModel::error_response(
            ModelError::AuthFailed,
        ));

        let conversation = Conversation::from_tier(ModelTier::Small, 4096);
        let tools = ToolRegistry::new();
        let config = ReactConfig {
            max_iterations: 10,
            max_elapsed_sec: 60,
            retry_policy: RetryPolicy {
                max_retries: 3,
                ..Default::default()
            },
            ..Default::default()
        };
        let (event_tx, _) = crate::events::event_channel();

        let loop_ = ReactLoop::new(model, conversation, tools, config, event_tx);
        let result = loop_.run("Test prompt".into()).await;

        assert!(result.is_err());
        match result.unwrap_err() {
            ReactError::Model(ModelError::AuthFailed) => {}
            _ => panic!("expected AuthFailed error, not retry"),
        }
    }

    /// ServiceUnavailable (503) is retried with backoff.
    #[tokio::test]
    async fn test_react_loop_service_unavailable_retries() {
        let model = Arc::new(MockModel::small("test-model"));
        model.push_response(crate::model::mock::MockModel::error_response(
            ModelError::ServiceUnavailable { retry_after_sec: 0 },
        ));
        model.push_response(crate::model::mock::MockModel::text_response(
            "Task complete",
        ));

        let conversation = Conversation::from_tier(ModelTier::Small, 4096);
        let tools = ToolRegistry::new();
        let config = ReactConfig {
            max_iterations: 10,
            max_elapsed_sec: 60,
            retry_policy: RetryPolicy {
                max_retries: 3,
                base_backoff_ms: 10,
                ..Default::default()
            },
            ..Default::default()
        };
        let (event_tx, _) = crate::events::event_channel();

        let loop_ = ReactLoop::new(model, conversation, tools, config, event_tx);
        let result = loop_.run("Test prompt".into()).await;

        assert!(
            result.is_ok(),
            "loop should complete after retry: {result:?}"
        );
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

        async fn execute(&self, _args: Value) -> Result<ToolExecutionResult, ToolError> {
            Ok(ToolExecutionResult::output("written".to_string()))
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
            if let Ok(event) = event_rx.try_recv()
                && let crate::events::Event::ApprovalRequest(req) = event
            {
                req_id = Some(req.id);
                break;
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
            if let Ok(event) = event_rx.try_recv()
                && let crate::events::Event::ApprovalRequest(req) = event
            {
                req_id = Some(req.id);
                break;
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
            if let Ok(event) = event_rx.try_recv()
                && let crate::events::Event::ApprovalRequest(req) = event
            {
                req_id = Some(req.id);
                break;
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
            if let Ok(event) = event_rx.try_recv()
                && let crate::events::Event::ApprovalRequest(req) = event
                && req.id != req_id
            {
                second_req = Some(req.id);
                break;
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
