//! ReAct loop — core control flow: think → tool → observe → repeat.

use crate::context::compaction::{CompactionConfig, CompactionManager};
use crate::context::{Conversation, ConversationError};
use crate::cost::{CostBudgetError, CostTracker};
use crate::events::{
    Approval, ApprovalDecision, ApprovalRequest, CompactionEvent, CompactionTrigger, Event,
    EventSender, LogLevel, Message, MessageRole, SubAgentComplete, SubAgentError, ToolCall,
    ToolCallStatus, ToolResult,
};
use crate::harness::retry::{RetryDecision, RetryPolicy};
use crate::model::text_tool::{TextToolError, TextToolParser};
use crate::model::{
    GenerationParams, Model, ModelCapabilities, ModelError, ModelMessage, ModelResponse, ModelRole,
    ModelTier,
};
use crate::session::SessionManager;
use crate::snapshot::store::{SnapshotEntry, SnapshotError, SnapshotStore};
use crate::tool::tool_trait::ToolCategory;
use crate::tool::{CacheConfig, ToolRegistry, ToolResultCache};
use anyhow::Context as _;
use futures::StreamExt;
use std::sync::{Arc, Mutex};
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
    #[error("cost budget: {0}")]
    CostBudget(#[from] CostBudgetError),
    #[error("selective context error: {0}")]
    SelectiveContext(#[from] crate::context::selective::SelectiveContextError),
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
    session: Option<Arc<Mutex<SessionManager>>>,
    /// Optional cost tracker for budget enforcement.
    cost_tracker: Option<CostTracker>,
    /// Optional snapshot store for git snapshots before destructive actions.
    snapshot_store: Option<Arc<Mutex<SnapshotStore>>>,
    /// Optional compaction manager for context compaction.
    compaction_manager: Option<CompactionManager>,
    /// Optional tool result cache for idempotent tools (read, grep).
    tool_cache: Option<ToolResultCache>,
    /// Event index counter for snapshot metadata.
    event_index: usize,
    /// Project root for selective context inclusion.
    project_root: Option<std::path::PathBuf>,
    /// Agent registry for sub-agent delegation.
    agent_registry: Option<Arc<crate::config::AgentRegistry>>,
    /// Active sub-agent handles for monitoring and cleanup.
    subagent_handles: Vec<crate::harness::SubAgentHandle>,
    /// Remaining spawn budget for sub-agent delegation (default 64).
    spawn_budget: u32,
    /// Current delegation depth.
    delegation_depth: u32,
}

impl<M: Model + 'static> ReactLoop<M> {
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
            cost_tracker: None,
            snapshot_store: None,
            compaction_manager: None,
            tool_cache: None,
            event_index: 0,
            project_root: None,
            agent_registry: None,
            subagent_handles: Vec::new(),
            spawn_budget: 64,
            delegation_depth: 0,
        }
    }

    /// Set the project root for selective context inclusion.
    pub fn with_project_root(mut self, project_root: std::path::PathBuf) -> Self {
        self.project_root = Some(project_root);
        self
    }

    /// Get the event sender for emitting events.
    pub fn event_tx(&self) -> EventSender {
        self.event_tx.clone()
    }

    /// Get the tool registry.
    pub fn tools(&self) -> &ToolRegistry {
        &self.tools
    }

    /// Get the model.
    pub fn model(&self) -> &Arc<M> {
        &self.model
    }

    /// Get the model capabilities.
    pub fn model_capabilities(&self) -> &ModelCapabilities {
        self.model.capabilities()
    }

    /// Get the ReactConfig.
    pub fn config(&self) -> &ReactConfig {
        &self.config
    }

    /// Get the cost tracker.
    pub fn cost_tracker(&self) -> Option<&CostTracker> {
        self.cost_tracker.as_ref()
    }

    /// Get the project root.
    pub fn project_root(&self) -> Option<&std::path::PathBuf> {
        self.project_root.as_ref()
    }

    /// Get the session manager.
    pub fn session(&self) -> Option<Arc<Mutex<SessionManager>>> {
        self.session.clone()
    }

    /// Get the snapshot store.
    pub fn snapshot_store(&self) -> Option<Arc<Mutex<SnapshotStore>>> {
        self.snapshot_store.clone()
    }

    /// Get the conversation.
    pub fn conversation(&self) -> &Conversation {
        &self.conversation
    }

    /// Get the current iteration count.
    pub fn iteration(&self) -> usize {
        self.iteration
    }

    /// Set the session manager for logging events to a session log.
    pub fn with_session(mut self, session: SessionManager) -> Self {
        self.session = Some(Arc::new(Mutex::new(session)));
        self
    }

    /// Set the session manager from an Arc (for sharing).
    pub fn with_session_arc(mut self, session: Arc<Mutex<SessionManager>>) -> Self {
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

    /// Set the cost tracker for budget enforcement.
    pub fn with_cost_tracker(mut self, tracker: CostTracker) -> Self {
        self.cost_tracker = Some(tracker);
        self
    }

    /// Set the snapshot store for git snapshots before destructive actions.
    pub fn with_snapshot_store(mut self, store: SnapshotStore) -> Self {
        self.snapshot_store = Some(Arc::new(Mutex::new(store)));
        self
    }

    /// Set the snapshot store from an Arc (for sharing).
    pub fn with_snapshot_store_arc(mut self, store: Arc<Mutex<SnapshotStore>>) -> Self {
        self.snapshot_store = Some(store);
        self
    }

    /// Set the compaction manager for context compaction.
    pub fn with_compaction(mut self, config: CompactionConfig) -> Self {
        self.compaction_manager = Some(CompactionManager::new(config));
        self
    }

    /// Set the tool result cache for idempotent tool calls (read, grep).
    pub fn with_tool_cache(mut self, config: CacheConfig) -> Self {
        self.tool_cache = Some(ToolResultCache::new(config));
        self
    }

    /// Set the agent registry for sub-agent delegation.
    pub fn with_agent_registry(mut self, registry: Arc<crate::config::AgentRegistry>) -> Self {
        self.agent_registry = Some(registry);
        self
    }

    /// Get the compaction manager config.
    pub fn compaction_config(&self) -> Option<CompactionConfig> {
        self.compaction_manager
            .as_ref()
            .map(|cm| cm.config().clone())
    }

    /// Get the tool cache config.
    pub fn tool_cache_config(&self) -> Option<CacheConfig> {
        self.tool_cache.as_ref().map(|tc| tc.config().clone())
    }

    /// Get the agent registry.
    pub fn agent_registry(&self) -> Option<Arc<crate::config::AgentRegistry>> {
        self.agent_registry.clone()
    }

    /// Get the current delegation depth.
    pub fn delegation_depth(&self) -> u32 {
        self.delegation_depth
    }

    /// Get the remaining spawn budget.
    pub fn spawn_budget(&self) -> u32 {
        self.spawn_budget
    }

    /// Run selective context inclusion to find relevant files for the current task.
    ///
    /// This uses the conversation's selective context configuration (based on model tier)
    /// to scan the project for files matching symbols/terms from the user prompt and
    /// recent conversation history. The included files are added to the conversation context.
    async fn run_selective_context(&mut self, prompt: &str) -> Result<(), ReactError> {
        let Some(project_root) = &self.project_root else {
            return Ok(()); // No project root configured, skip selective context
        };

        // Build search terms from prompt + recent conversation
        let search_terms = self.build_search_terms(prompt);
        if search_terms.is_empty() {
            return Ok(());
        }

        // Get selective context config from conversation
        let selective_config = self.conversation.selective_config().clone();
        let token_counter = crate::context::tokens::TokenCounter;

        // Find relevant files
        let included_files = crate::context::selective::find_relevant_files(
            project_root,
            &search_terms,
            &selective_config,
            &token_counter,
        )?;

        if !included_files.is_empty() {
            self.conversation.set_included_files(included_files);
            // Emit log about files included
            let count = self.conversation.included_files().len();
            let total_tokens: usize = self
                .conversation
                .included_files()
                .iter()
                .map(|f| f.tokens)
                .sum();
            self.emit_log(
                crate::events::LogLevel::Debug,
                format!(
                    "Selective context: included {} files ({} tokens)",
                    count, total_tokens
                ),
            )
            .await;
        }

        Ok(())
    }

    /// Build search terms from the current prompt and recent conversation history.
    fn build_search_terms(&self, prompt: &str) -> String {
        // Start with terms from the current prompt
        let mut terms = crate::context::selective::extract_search_terms(prompt);

        // Add terms from recent user messages (last 3)
        let recent_user_messages: Vec<_> = self
            .conversation
            .messages()
            .iter()
            .rev()
            .filter(|m| m.role == crate::events::MessageRole::User)
            .take(3)
            .collect();

        for msg in recent_user_messages {
            let msg_terms = crate::context::selective::extract_search_terms(&msg.content);
            terms.extend(msg_terms);
        }

        // Deduplicate
        terms.sort();
        terms.dedup();
        terms.join("|")
    }

    /// Undo to the previous snapshot.
    pub fn undo(&mut self) -> Result<SnapshotEntry, SnapshotError> {
        let Some(store) = &self.snapshot_store else {
            return Err(SnapshotError::NoSnapshots);
        };
        store.lock().unwrap().undo()
    }

    /// Redo to the next snapshot.
    pub fn redo(&mut self) -> Result<SnapshotEntry, SnapshotError> {
        let Some(store) = &self.snapshot_store else {
            return Err(SnapshotError::NoSnapshots);
        };
        store.lock().unwrap().redo()
    }

    /// Check if undo is available.
    pub fn can_undo(&self) -> bool {
        self.snapshot_store
            .as_ref()
            .map(|s| s.lock().unwrap().can_undo())
            .unwrap_or(false)
    }

    /// Check if redo is available.
    pub fn can_redo(&self) -> bool {
        self.snapshot_store
            .as_ref()
            .map(|s| s.lock().unwrap().can_redo())
            .unwrap_or(false)
    }

    /// Manually trigger context compaction.
    ///
    /// This can be called from slash commands (e.g., `/compact`) to force
    /// compaction regardless of the current token usage.
    pub async fn compact_context(
        &mut self,
    ) -> Result<Option<crate::context::compaction::CompactionResult>, ReactError> {
        if let Some(compaction_manager) = &mut self.compaction_manager {
            let result = compaction_manager
                .maybe_compact(
                    &*self.model,
                    &mut self.conversation,
                    CompactionTrigger::Manual,
                )
                .await
                .map_err(|e| ReactError::Other(anyhow::anyhow!(e)))?;

            if let Some(ref result) = result {
                // Emit compaction event
                let compaction_event = CompactionEvent {
                    trigger: CompactionTrigger::Manual,
                    strategy: result.strategy_used.clone(),
                    tokens_before: result.tokens_before,
                    tokens_after: result.tokens_after,
                    messages_removed: result.messages_removed,
                    turns_summarized: result.turns_summarized,
                    native_provider: result.native_provider.clone(),
                    summary: result.summary.clone(),
                    ts: current_timestamp(),
                };
                self.emit_compaction(compaction_event).await;
            }

            Ok(result)
        } else {
            Ok(None)
        }
    }

    /// Run the ReAct loop until completion or termination.
    pub async fn run(&mut self, initial_prompt: String) -> Result<String, ReactError> {
        // Add initial user message
        self.conversation
            .add_user_message(initial_prompt.clone())
            .context("failed to add initial prompt")?;

        // Run selective context inclusion for the initial prompt
        self.run_selective_context(&initial_prompt).await?;

        // Emit initial status
        self.emit_status().await;

        // Take initial snapshot
        self.take_snapshot("Session start").await?;

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
            // Check for completed sub-agents
            self.check_subagent_completion().await?;

            // Check shutdown signal
            if shutdown_rx.try_recv().is_ok() {
                self.emit_interrupted(false).await; // Signal-based (SIGINT)
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

            // Check and perform context compaction if needed
            if let Some(compaction_manager) = &mut self.compaction_manager
                && let Some(result) = compaction_manager
                    .maybe_compact(
                        &*self.model,
                        &mut self.conversation,
                        CompactionTrigger::Auto,
                    )
                    .await
                    .map_err(|e| ReactError::Other(anyhow::anyhow!(e)))?
            {
                // Emit compaction event
                let compaction_event = CompactionEvent {
                    trigger: CompactionTrigger::Auto,
                    strategy: result.strategy_used.clone(),
                    tokens_before: result.tokens_before,
                    tokens_after: result.tokens_after,
                    messages_removed: result.messages_removed,
                    turns_summarized: result.turns_summarized,
                    native_provider: result.native_provider.clone(),
                    summary: result.summary.clone(),
                    ts: current_timestamp(),
                };
                self.emit_compaction(compaction_event).await;
            }

            // Run one iteration
            match self.iterate(&mut shutdown_rx).await? {
                LoopDecision::Continue => {
                    self.iteration += 1;
                    // Take snapshot at loop boundary
                    self.take_boundary_snapshot().await?;
                    continue;
                }
                LoopDecision::Complete(result) => {
                    // Take final snapshot before completing
                    self.take_snapshot("Session complete").await?;
                    return Ok(result);
                }
                LoopDecision::Stop(reason) => {
                    return Err(ReactError::Stopped { reason });
                }
            }
        }
    }

    /// Run exactly one ReAct turn with the given prompt.
    ///
    /// This is the single-turn entry point used by the TUI in interactive mode.
    /// The TUI owns the input loop and calls this method once per user prompt.
    /// Returns the LoopDecision so the caller can decide whether to continue.
    ///
    /// Unlike `run()`, this method does NOT:
    /// - Set up signal handlers
    /// - Loop until completion
    /// - Take session start/end snapshots automatically
    ///
    /// The caller is responsible for checking iteration/time limits and context caps.
    /// The caller provides a shutdown receiver to allow interrupting the model generation.
    pub async fn run_turn(
        &mut self,
        prompt: String,
        shutdown_rx: mpsc::Receiver<()>,
    ) -> Result<LoopDecision, ReactError> {
        // Add user message to conversation
        self.conversation
            .add_user_message(prompt.clone())
            .context("failed to add user prompt")?;

        // Run selective context inclusion to find relevant files
        self.run_selective_context(&prompt).await?;

        // Emit status update
        self.emit_status().await;

        // Check limits before iterating
        if self.iteration >= self.config.max_iterations {
            return Err(ReactError::MaxIterations {
                max_iter: self.config.max_iterations,
            });
        }
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

        // Check and perform context compaction if needed
        if let Some(compaction_manager) = &mut self.compaction_manager
            && let Some(result) = compaction_manager
                .maybe_compact(
                    &*self.model,
                    &mut self.conversation,
                    CompactionTrigger::Auto,
                )
                .await
                .map_err(|e| ReactError::Other(anyhow::anyhow!(e)))?
        {
            // Emit compaction event
            let compaction_event = CompactionEvent {
                trigger: CompactionTrigger::Auto,
                strategy: result.strategy_used.clone(),
                tokens_before: result.tokens_before,
                tokens_after: result.tokens_after,
                messages_removed: result.messages_removed,
                turns_summarized: result.turns_summarized,
                native_provider: result.native_provider.clone(),
                summary: result.summary.clone(),
                ts: current_timestamp(),
            };
            self.emit_compaction(compaction_event).await;
        }

        // Run exactly one iteration with the provided shutdown receiver
        let mut shutdown_rx = shutdown_rx;
        let decision = self.iterate(&mut shutdown_rx).await?;

        match &decision {
            LoopDecision::Continue => {
                self.iteration += 1;
                // Take snapshot at loop boundary
                self.take_boundary_snapshot().await?;
            }
            LoopDecision::Complete(_) => {
                // Take final snapshot
                self.take_snapshot("Session complete").await?;
            }
            LoopDecision::Stop(_) => {}
        }

        Ok(decision)
    }

    /// Create a shutdown channel for a turn and return the sender to the caller.
    /// The caller can use the sender to interrupt the turn (e.g., on Esc key press).
    pub fn create_turn_shutdown(&self) -> (mpsc::Sender<()>, mpsc::Receiver<()>) {
        mpsc::channel(1)
    }

    /// Run a single ReAct iteration: think → tool → observe.
    async fn iterate(
        &mut self,
        shutdown_rx: &mut mpsc::Receiver<()>,
    ) -> Result<LoopDecision, ReactError> {
        // THINK: Get model completion
        let model_messages = self.build_model_messages();
        let response = self.call_model(&model_messages, shutdown_rx).await?;

        // Record model usage cost
        if let Some(usage) = &response.usage {
            self.record_model_usage(usage).await;
        }

        // Add assistant message to conversation
        self.conversation
            .add_assistant_message(
                response.content.clone(),
                response.tool_calls.clone(),
                response.thinking.clone(),
            )
            .context("failed to add assistant message")?;

        // Emit assistant message event
        self.emit_message(Message {
            role: MessageRole::Assistant,
            content: response.content.clone(),
            tool_calls: response.tool_calls.clone(),
            tool_call_id: None,
            ts: current_timestamp(),
            thinking: response.thinking.clone(),
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
                let simpler_response = self.call_model(&simpler_messages, shutdown_rx).await?;

                // Add assistant message from fallback attempt.
                self.conversation
                    .add_assistant_message(
                        simpler_response.content.clone(),
                        simpler_response.tool_calls.clone(),
                        simpler_response.thinking.clone(),
                    )
                    .context("failed to add fallback assistant message")?;
                self.emit_message(Message {
                    role: MessageRole::Assistant,
                    content: simpler_response.content.clone(),
                    tool_calls: simpler_response.tool_calls.clone(),
                    tool_call_id: None,
                    ts: current_timestamp(),
                    thinking: simpler_response.thinking.clone(),
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
            thinking: None,
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
                // For simpler prompt (Small tier), don't include thinking
                thinking: None,
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
    ///
    /// This method uses streaming and is interruptible via the shutdown receiver.
    /// If the shutdown signal is received, the in-flight request is aborted and
    /// `ReactError::Interrupted` is returned.
    async fn call_model(
        &mut self,
        messages: &[ModelMessage],
        shutdown_rx: &mut mpsc::Receiver<()>,
    ) -> Result<ModelResponse, ReactError> {
        let retry_policy = self.config.retry_policy.clone();
        let mut attempt = 0usize;

        // Pass native tool specs only when the model supports native function calling.
        // Models without native FC use text-based parsing via the system prompt snippet.
        let native_tools = if self.model.capabilities().tool_calling {
            Some(self.tools.specs())
        } else {
            None
        };

        loop {
            // Get a streaming response from the model
            let stream_result = tokio::select! {
                biased;
                result = self.model.stream(
                    messages,
                    &self.config.generation_params,
                    native_tools.as_deref(),
                ) => result,
                _ = shutdown_rx.recv() => {
                    // Interrupt signal received - abort the request
                    self.emit_log(
                        LogLevel::Warn,
                        "Model generation interrupted by user".to_string(),
                    )
                    .await;
                    self.emit_interrupted(false).await;
                    return Err(ReactError::Interrupted);
                }
            };

            let mut stream = match stream_result {
                Ok(s) => s,
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
                    return Err(ReactError::Model(error));
                }
            };

            // Consume the stream, emitting Token events and building the response
            let mut content = String::new();
            let mut tool_calls: Option<Vec<ToolCall>> = None;
            let mut thinking: Option<String> = None;
            let mut usage: Option<crate::model::Usage> = None;
            let mut finished = false;

            while !finished {
                let chunk_result = tokio::select! {
                    biased;
                    chunk = stream.next() => chunk,
                    _ = shutdown_rx.recv() => {
                        // Interrupt signal received during streaming
                        self.emit_log(
                            LogLevel::Warn,
                            "Model generation interrupted by user".to_string(),
                        )
                        .await;
                        self.emit_interrupted(false).await;
                        return Err(ReactError::Interrupted);
                    }
                };

                match chunk_result {
                    Some(Ok(chunk)) => {
                        // Emit token delta event for TUI streaming render
                        if !chunk.delta.is_empty() {
                            let token_event = crate::events::Event::token(
                                chunk.delta.clone(),
                                current_timestamp(),
                            );
                            let _ = self.event_tx.send(token_event.clone());
                            if let Some(session) = &self.session {
                                let _ = session.lock().unwrap().log_event(token_event);
                            }
                            content.push_str(&chunk.delta);
                        }

                        if let Some(tc) = chunk.tool_calls {
                            tool_calls = Some(tc);
                        }
                        if let Some(th) = chunk.thinking {
                            thinking = Some(th.clone());
                            // Emit thinking event for TUI/headless parity
                            let thinking_event =
                                crate::events::Event::thinking(th, current_timestamp());
                            let _ = self.event_tx.send(thinking_event.clone());
                            if let Some(session) = &self.session {
                                let _ = session.lock().unwrap().log_event(thinking_event);
                            }
                        }
                        if chunk.finished {
                            finished = true;
                            // Usage is not available in streaming chunks; we'd need a final response
                            // For now, estimate from content
                            usage = Some(crate::model::Usage {
                                prompt_tokens: 0, // Would need to be computed
                                completion_tokens: crate::context::tokens::TokenCounter
                                    .count(&content),
                                total_tokens: crate::context::tokens::TokenCounter.count(&content),
                            });
                        }
                    }
                    Some(Err(e)) => return Err(ReactError::Model(e)),
                    None => {
                        // Stream ended without finished=true
                        finished = true;
                        usage = Some(crate::model::Usage {
                            prompt_tokens: 0,
                            completion_tokens: crate::context::tokens::TokenCounter.count(&content),
                            total_tokens: crate::context::tokens::TokenCounter.count(&content),
                        });
                    }
                }
            }

            return Ok(ModelResponse {
                content,
                tool_calls,
                thinking,
                usage,
            });
        }
    }

    /// Emit an interrupted event to the event stream.
    async fn emit_interrupted(&mut self, user_initiated: bool) {
        let event = crate::events::Event::interrupted(user_initiated);
        let _ = self.event_tx.send(event.clone());
        if let Some(session) = &self.session {
            let _ = session.lock().unwrap().log_event(event);
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
    ///
    /// Takes a git snapshot before executing destructive tools (Write, Delete, Network).
    /// Checks tool result cache for idempotent tools (read, grep) before execution.
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

        // Check tool result cache for idempotent tools (read, grep) before policy/cost checks.
        // This avoids unnecessary approval prompts and cost checks for cached results.
        if let Some(cache) = &mut self.tool_cache
            && let Some(cached_exec_result) = cache.get(&tool_call.name, &tool_call.args)
        {
            // Cache hit - emit log and return cached result
            self.emit_log(
                LogLevel::Debug,
                format!(
                    "Tool result cache hit for {} (skipping execution)",
                    tool_call.name
                ),
            )
            .await;

            let duration_ms = 0; // cached result has no execution time
            let result = ToolResult {
                id: tool_call.id,
                name: tool_call.name,
                args: tool_call.args,
                output: cached_exec_result.output,
                ok: true,
                duration_ms,
                truncated: cached_exec_result.truncated,
                exit_code: cached_exec_result.exit_code,
                timed_out: cached_exec_result.timed_out,
                cancelled: cached_exec_result.cancelled,
            };

            // Record actual cost after tool execution (zero for cached).
            self.record_tool_cost(&result).await;

            // Emit tool call completed event
            self.emit_tool_call(ToolCall {
                id: result.id.clone(),
                name: result.name.clone(),
                args: result.args.clone(),
                status: ToolCallStatus::Ok,
            })
            .await;

            return Ok(result);
        }

        // Handle sub-agent delegation via todo tool with "delegate" action.
        if tool_call.name == "todo"
            && let Ok(crate::tool::todo::TodoArgs::Delegate {
                todo_id,
                agent,
                prompt,
            }) = serde_json::from_value::<crate::tool::todo::TodoArgs>(tool_call.args.clone())
        {
            return self
                .handle_delegate(tool_call.id, todo_id, agent, prompt)
                .await;
        }

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

        // Check expensive call confirmation before executing the tool.
        self.check_expensive_call_confirmation(&tool_call).await?;

        // Check cost budget before executing the tool.
        self.check_cost_budget(&tool_call).await?;

        // Take a snapshot before destructive actions (Write, Delete, Network).
        // We do this after approval is granted but before execution.
        if matches!(
            spec.category,
            ToolCategory::Write | ToolCategory::Delete | ToolCategory::Network
        ) {
            self.take_snapshot(&format!("Before {} tool", tool_call.name))
                .await?;
        }

        // Emit tool call started event (before re-fetching tool to avoid borrow conflict)
        self.emit_tool_call(ToolCall {
            id: tool_call.id.clone(),
            name: tool_call.name.clone(),
            args: tool_call.args.clone(),
            status: ToolCallStatus::Pending,
        })
        .await;

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

        // Clone exec_result for caching before we move its output into result.
        let exec_result_for_cache = exec_result.clone();

        let result = ToolResult {
            id: tool_call.id,
            name: tool_call.name.clone(), // clone for cache invalidation below
            args: tool_call.args.clone(), // clone for cache invalidation below
            output: exec_result.output,
            ok: true,
            duration_ms,
            truncated: exec_result.truncated,
            exit_code: exec_result.exit_code,
            timed_out: exec_result.timed_out,
            cancelled: exec_result.cancelled,
        };

        // Store result in cache for idempotent tools (read, grep).
        if let Some(cache) = &mut self.tool_cache {
            cache.put(&tool_call.name, &tool_call.args, &exec_result_for_cache);
        }

        // Invalidate cache for paths affected by write/delete/run operations.
        if let Some(cache) = &mut self.tool_cache
            && matches!(
                spec.category,
                ToolCategory::Write | ToolCategory::Delete | ToolCategory::Network
            )
        {
            // Extract paths from tool args and invalidate cache for those paths
            use crate::tool::cache::extract_paths;
            for path in extract_paths(&tool_call.name, &tool_call.args) {
                cache.invalidate_path(&path);
            }
            // For run tool, also invalidate all since it can modify arbitrary files
            if tool_call.name == "run" {
                cache.invalidate_all();
            }
        }

        // Record actual cost after tool execution.
        self.record_tool_cost(&result).await;

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

    /// Handle sub-agent delegation via todo tool.
    async fn handle_delegate(
        &mut self,
        tool_call_id: String,
        todo_id: String,
        agent_name: String,
        prompt: String,
    ) -> Result<ToolResult, ReactError> {
        let Some(agent_registry) = self.agent_registry.clone() else {
            return Err(ReactError::Tool(
                "agent registry not configured; cannot delegate".to_string(),
            ));
        };

        // Check delegation depth
        if self.delegation_depth >= 1 {
            return Err(ReactError::Tool(format!(
                "maximum delegation depth ({}) exceeded",
                1
            )));
        }

        // Check spawn budget
        if self.spawn_budget == 0 {
            return Err(ReactError::Tool(
                "spawn budget exhausted; cannot delegate".to_string(),
            ));
        }

        // Validate agent profile exists
        let Some(profile) = agent_registry.get(&agent_name) else {
            return Err(ReactError::Tool(format!(
                "agent profile not found: {}",
                agent_name
            )));
        };

        // Check profile max_depth
        if self.delegation_depth >= profile.max_depth {
            return Err(ReactError::Tool(format!(
                "maximum delegation depth for agent '{}' exceeded: current={}, max={}",
                agent_name, self.delegation_depth, profile.max_depth
            )));
        }

        // Spawn the sub-agent
        let handle = crate::harness::spawn_subagent(
            self,
            agent_registry,
            todo_id.clone(),
            agent_name.clone(),
            prompt.clone(),
            self.delegation_depth,
            self.spawn_budget,
        )
        .await
        .map_err(|e| ReactError::Tool(format!("failed to spawn sub-agent: {}", e)))?;

        // Decrement spawn budget
        self.spawn_budget -= 1;

        // Store handle for monitoring
        let subagent_id = handle.subagent_id.clone();
        self.subagent_handles.push(handle);

        // Return the sub-agent handle as the tool result
        let output = serde_json::to_string(&serde_json::json!({
            "subagent_id": subagent_id,
            "todo_id": todo_id,
            "agent": agent_name,
            "status": "spawned"
        }))
        .unwrap();

        Ok(ToolResult {
            id: tool_call_id,
            name: "subagent".to_string(),
            args: serde_json::json!({ "todo_id": todo_id, "agent": agent_name, "prompt": prompt }),
            output,
            ok: true,
            duration_ms: 0,
            truncated: false,
            exit_code: Some(0),
            timed_out: false,
            cancelled: false,
        })
    }

    /// Check cost budget before executing a tool.
    async fn check_cost_budget(&mut self, tool_call: &ToolCall) -> Result<(), ReactError> {
        let Some(tracker) = &mut self.cost_tracker else {
            return Ok(()); // Cost tracking not enabled
        };

        // Estimate input tokens from tool call args
        let input_tokens = crate::context::tokens::TokenCounter.count(&tool_call.args.to_string());
        // Estimate output tokens (rough heuristic: 1000 tokens for tool result)
        let estimated_output_tokens = 1000;

        // Check session cap
        let estimated_cost = tracker
            .estimate_tool_call_cost(input_tokens, estimated_output_tokens)
            .map_err(ReactError::CostBudget)?;

        tracker
            .check_session_cap(estimated_cost.total_cost_usd())
            .map_err(ReactError::CostBudget)?;

        // Check per-turn warning (soft warning, doesn't block)
        tracker
            .check_per_turn_warning(estimated_cost.total_cost_usd(), &self.event_tx)
            .await;

        Ok(())
    }

    /// Check if a tool call requires expensive call confirmation.
    ///
    /// If the estimated cost exceeds the configured threshold, emits an ApprovalRequest
    /// and waits for user decision. Returns an error if denied.
    async fn check_expensive_call_confirmation(
        &mut self,
        tool_call: &ToolCall,
    ) -> Result<(), ReactError> {
        let Some(tracker) = &self.cost_tracker else {
            return Ok(()); // Cost tracking not enabled
        };

        // Estimate input tokens from tool call args
        let input_tokens = crate::context::tokens::TokenCounter.count(&tool_call.args.to_string());
        // Estimate output tokens (rough heuristic: 1000 tokens for tool result)
        let estimated_output_tokens = 1000;

        // Check if estimated cost exceeds expensive call confirmation threshold
        let estimated_cost =
            match tracker.estimate_tool_call_cost(input_tokens, estimated_output_tokens) {
                Ok(cost) => cost,
                Err(_) => return Ok(()), // Unknown pricing - fail open for confirmation (budget check handles fail-closed)
            };

        if !tracker.requires_expensive_call_confirmation(estimated_cost.total_cost_usd()) {
            return Ok(()); // Below threshold, no confirmation needed
        }

        // Check if tool has session allow-always grant (skip confirmation)
        if self.policy.has_session_allow(&tool_call.name) {
            return Ok(());
        }

        // Emit approval request and pause the loop until a decision arrives.
        let approval_id = format!("approval-expensive-{}", self.iteration);
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
                    "permission denied (expensive call): {}",
                    tool_call.name
                )));
            }
        }

        Ok(())
    }

    /// Record actual cost after tool execution.
    async fn record_tool_cost(&mut self, result: &ToolResult) {
        let Some(tracker) = &mut self.cost_tracker else {
            return; // Cost tracking not enabled
        };

        // Estimate tokens from tool result output
        let output_tokens = crate::context::tokens::TokenCounter.count(&result.output);
        // Input tokens were the tool call args
        let input_tokens = crate::context::tokens::TokenCounter.count(&result.args.to_string());

        if let Ok(cost) = tracker.estimate_tool_call_cost(input_tokens, output_tokens) {
            tracker.record_cost(&cost);
        }
    }

    /// Record model usage cost from a completion response.
    async fn record_model_usage(&mut self, usage: &crate::model::Usage) {
        let Some(tracker) = &mut self.cost_tracker else {
            return; // Cost tracking not enabled
        };
        tracker.record_usage(usage);
    }

    /// Get the todo list from the todo tool (if available).
    fn get_todo_list(&self) -> Option<crate::tool::todo::TodoList> {
        self.tools.get("todo").and_then(|tool| {
            // Downcast to TodoTool to access the list
            tool.as_any()
                .downcast_ref::<crate::tool::todo::TodoTool>()
                .map(|todo_tool| todo_tool.list.clone())
        })
    }

    /// Check for completed sub-agents and handle their completion.
    async fn check_subagent_completion(&mut self) -> Result<(), ReactError> {
        let mut completed_indices = Vec::new();

        // Check each sub-agent handle for completion
        for (index, handle) in self.subagent_handles.iter().enumerate() {
            if handle.join_handle.is_finished() {
                completed_indices.push(index);
            }
        }

        // Process completed sub-agents in reverse order to maintain indices
        for index in completed_indices.into_iter().rev() {
            let handle = self.subagent_handles.remove(index);
            let subagent_id = handle.subagent_id.clone();
            let todo_id = handle.todo_id.clone();

            // Wait for the sub-agent result
            match handle.join_handle.await {
                Ok(Ok(result)) => {
                    // Sub-agent completed successfully
                    let _summary =
                        format!("Sub-agent '{}' completed: {}", subagent_id, result.summary);

                    // Emit SubAgentComplete event
                    let complete_event = SubAgentComplete {
                        subagent_id: subagent_id.clone(),
                        todo_id: todo_id.clone(),
                        summary: result.summary.clone(),
                        tokens_used: result.tokens_used,
                        result: result.result.clone(),
                        ts: current_timestamp(),
                    };
                    self.emit_subagent_complete(complete_event).await;

                    // Complete the todo item
                    if let Some(todo_list) = self.get_todo_list() {
                        let _ = todo_list.complete(&todo_id);
                    }

                    // Inject the sub-agent result into the parent conversation
                    self.conversation
                        .add_tool_message(result.result.output.clone(), result.result.id.clone())
                        .context("failed to add sub-agent result to conversation")?;

                    // Emit tool result event for the sub-agent
                    self.emit_tool_result(result.result).await;

                    // Log completion
                    self.emit_log(
                        LogLevel::Info,
                        format!(
                            "Sub-agent '{}' completed (todo: {}) in ~{} tokens",
                            subagent_id, todo_id, result.tokens_used
                        ),
                    )
                    .await;
                }
                Ok(Err(e)) => {
                    // Sub-agent returned an error result
                    self.emit_log(
                        LogLevel::Error,
                        format!("Sub-agent '{}' failed: {}", subagent_id, e),
                    )
                    .await;

                    // Emit error event
                    let error_event = SubAgentError {
                        subagent_id: subagent_id.clone(),
                        code: "execution_failed".to_string(),
                        message: e.to_string(),
                        ts: current_timestamp(),
                    };
                    self.emit_subagent_error(error_event).await;
                }
                Err(e) => {
                    // Join error (task panicked)
                    self.emit_log(
                        LogLevel::Error,
                        format!("Sub-agent '{}' panicked: {}", subagent_id, e),
                    )
                    .await;

                    let error_event = SubAgentError {
                        subagent_id: subagent_id.clone(),
                        code: "panicked".to_string(),
                        message: e.to_string(),
                        ts: current_timestamp(),
                    };
                    self.emit_subagent_error(error_event).await;
                }
            }
        }

        Ok(())
    }

    /// Take a git snapshot with the given description.
    ///
    /// Increments the event index and stores the snapshot metadata.
    /// Emits SnapshotProgress events for UI integration.
    async fn take_snapshot(&mut self, description: &str) -> Result<(), ReactError> {
        let Some(store) = &self.snapshot_store else {
            return Ok(()); // Snapshots not enabled
        };

        let event_index = self.event_index;
        self.event_index += 1;

        // Emit progress event
        let progress = crate::events::SnapshotProgress {
            phase: "Creating snapshot".to_string(),
            percent: 0.0,
            elapsed_ms: self.start_time.elapsed().as_millis() as u64,
        };
        let _ = self.event_tx.send(Event::SnapshotProgress(progress));

        // Create the snapshot - release the lock before calling emit_log
        let snapshot_result = store.lock().unwrap().snapshot(description, event_index);

        match snapshot_result {
            Ok(snapshot_id) => {
                // Emit completion progress event
                let progress = crate::events::SnapshotProgress {
                    phase: "Snapshot created".to_string(),
                    percent: 100.0,
                    elapsed_ms: self.start_time.elapsed().as_millis() as u64,
                };
                let _ = self.event_tx.send(Event::SnapshotProgress(progress));

                // Log the snapshot creation
                self.emit_log(
                    LogLevel::Debug,
                    format!(
                        "Created snapshot: {} ({})",
                        &snapshot_id[..8.min(snapshot_id.len())],
                        description
                    ),
                )
                .await;

                Ok(())
            }
            Err(e) => {
                // Log but don't fail the loop if snapshot creation fails
                self.emit_log(LogLevel::Warn, format!("Failed to create snapshot: {}", e))
                    .await;
                Ok(())
            }
        }
    }

    /// Take a snapshot at a loop boundary (after each iteration).
    async fn take_boundary_snapshot(&mut self) -> Result<(), ReactError> {
        self.take_snapshot("Loop boundary").await
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
        if let Some(session) = &self.session {
            let _ = session.lock().unwrap().log_event(event);
        }
    }

    /// Emit a tool call event.
    async fn emit_tool_call(&mut self, call: ToolCall) {
        let event = Event::ToolCall(call);
        let _ = self.event_tx.send(event.clone());
        if let Some(session) = &self.session {
            let _ = session.lock().unwrap().log_event(event);
        }
    }

    /// Emit a tool result event.
    async fn emit_tool_result(&mut self, result: ToolResult) {
        let event = Event::ToolResult(result);
        let _ = self.event_tx.send(event.clone());
        if let Some(session) = &self.session {
            let _ = session.lock().unwrap().log_event(event);
        }
    }

    /// Emit a sub-agent completion event.
    async fn emit_subagent_complete(&mut self, event: SubAgentComplete) {
        let event = Event::SubAgentComplete(event);
        let _ = self.event_tx.send(event.clone());
        if let Some(session) = &self.session {
            let _ = session.lock().unwrap().log_event(event);
        }
    }

    /// Emit a sub-agent error event.
    async fn emit_subagent_error(&mut self, event: SubAgentError) {
        let event = Event::SubAgentError(event);
        let _ = self.event_tx.send(event.clone());
        if let Some(session) = &self.session {
            let _ = session.lock().unwrap().log_event(event);
        }
    }

    /// Emit an approval request event.
    async fn emit_approval_request(&mut self, req: ApprovalRequest) {
        let event = Event::ApprovalRequest(req);
        let _ = self.event_tx.send(event.clone());
        if let Some(session) = &self.session {
            let _ = session.lock().unwrap().log_event(event);
        }
    }

    /// Emit a status event.
    async fn emit_status(&mut self) {
        let cost_usd = if let Some(tracker) = &self.cost_tracker {
            tracker.spent_usd()
        } else {
            self.conversation.cost_estimate().total_cost_usd()
        };

        let status = crate::events::Status {
            model: self.model.model_id().to_string(),
            provider_configured: !self.model.model_id().is_empty(),
            tokens_in: self.conversation.estimated_tokens() as u64,
            tokens_out: 0,
            cost_usd,
            elapsed_ms: self.start_time.elapsed().as_millis() as u64,
            retries: self.retries,
            mode: "headless".to_string(),
        };
        let event = Event::Status(status);
        let _ = self.event_tx.send(event.clone());
        if let Some(session) = &self.session {
            let _ = session.lock().unwrap().log_event(event);
        }
    }

    /// Emit an error event to the event stream.
    async fn emit_error(&mut self, code: &str, message: &str) {
        let event = Event::error(code.to_string(), message.to_string());
        let _ = self.event_tx.send(event.clone());
        if let Some(session) = &self.session {
            let _ = session.lock().unwrap().log_event(event);
        }
    }

    /// Emit a log event.
    async fn emit_log(&mut self, level: crate::events::LogLevel, message: String) {
        let event = Event::Log(crate::events::Log { level, message });
        let _ = self.event_tx.send(event.clone());
        if let Some(session) = &self.session {
            let _ = session.lock().unwrap().log_event(event);
        }
    }

    /// Emit a compaction event.
    async fn emit_compaction(&mut self, event: CompactionEvent) {
        let event = Event::Compaction(event);
        let _ = self.event_tx.send(event.clone());
        if let Some(session) = &self.session {
            let _ = session.lock().unwrap().log_event(event);
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
