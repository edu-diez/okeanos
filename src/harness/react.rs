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
    /// Optional cheaper model used to write compaction summaries.
    summarizer: Option<Arc<dyn Model>>,
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
    /// Summary being computed in the background, if any.
    pending_compaction: Option<PendingCompaction>,
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
    /// Signature of the previous tool call and its consecutive repeat count.
    last_call_sig: Option<u64>,
    repeat_count: u32,
    /// Whether the last model response was cut off by the output token limit.
    last_truncated: bool,
}

/// Identical consecutive tool calls before the model is told to change course.
const DOOM_LOOP_WARN: u32 = 3;
/// Identical consecutive tool calls before the loop is stopped.
const DOOM_LOOP_STOP: u32 = 5;

/// Aborts the wrapped task when dropped so a finished `run()` doesn't leak
/// its signal handler.
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// A summary being computed off the main loop, with the plan it was made for.
struct PendingCompaction {
    plan: crate::context::CompactionPlan,
    handle: tokio::task::JoinHandle<Result<String, crate::context::CompactionError>>,
}

fn tool_result_from_exec(
    tool_call: &ToolCall,
    exec: crate::tool::tool_trait::ToolExecutionResult,
    duration_ms: u64,
) -> ToolResult {
    ToolResult {
        id: tool_call.id.clone(),
        name: tool_call.name.clone(),
        args: tool_call.args.clone(),
        output: exec.output,
        ok: true,
        duration_ms,
        truncated: exec.truncated,
        exit_code: exec.exit_code,
        timed_out: exec.timed_out,
        cancelled: exec.cancelled,
    }
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
        let mut conversation = conversation;
        let specs = tools.specs();
        let tool_overhead = if model.capabilities().tool_calling {
            serde_json::to_string(&specs).map_or(0, |s| s.len() / 4)
        } else if specs.is_empty() {
            0
        } else {
            TextToolParser::generate_system_prompt_snippet(&specs).len() / 4
        };
        conversation.set_tool_overhead(tool_overhead);
        Self {
            model,
            summarizer: None,
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
            pending_compaction: None,
            tool_cache: None,
            event_index: 0,
            project_root: None,
            agent_registry: None,
            subagent_handles: Vec::new(),
            spawn_budget: 64,
            delegation_depth: 0,
            last_call_sig: None,
            repeat_count: 0,
            last_truncated: false,
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

    /// Use a different (typically cheaper) model to write compaction summaries.
    pub fn with_summarizer_model(mut self, model: Arc<dyn Model>) -> Self {
        self.summarizer = Some(model);
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
    /// Expand the prompt with file context.
    ///
    /// Files mentioned as `@path` are always appended to the user message;
    /// keyword scanning of the project only runs when `auto_scan` is enabled.
    /// Context goes into the user message (not the system prompt) so the
    /// cached prefix stays stable.
    async fn expand_prompt(&mut self, prompt: &str) -> String {
        let Some(project_root) = self.project_root.clone() else {
            return prompt.to_string();
        };
        let config = self.conversation.selective_config().clone();
        let counter = crate::context::tokens::TokenCounter;

        let mut files =
            crate::context::selective::expand_mentions(&project_root, prompt, &config, &counter);

        if config.auto_scan {
            let terms = self.build_search_terms(prompt);
            if !terms.is_empty() {
                let cfg = config.clone();
                let root = project_root.clone();
                let scanned = tokio::task::spawn_blocking(move || {
                    crate::context::selective::find_relevant_files(
                        &root,
                        &terms,
                        &cfg,
                        &crate::context::tokens::TokenCounter,
                    )
                })
                .await;
                match scanned {
                    Ok(Ok(found)) => {
                        for f in found {
                            if files.len() < config.max_files
                                && !files.iter().any(|x| x.path == f.path)
                            {
                                files.push(f);
                            }
                        }
                    }
                    Ok(Err(e)) => {
                        self.emit_log(
                            LogLevel::Debug,
                            format!("Selective context scan failed: {e}"),
                        )
                        .await;
                    }
                    Err(_) => {}
                }
            }
        }

        if files.is_empty() {
            return prompt.to_string();
        }
        let total_tokens: usize = files.iter().map(|f| f.tokens).sum();
        self.emit_log(
            LogLevel::Debug,
            format!(
                "Selective context: included {} files ({} tokens)",
                files.len(),
                total_tokens
            ),
        )
        .await;
        format!(
            "{prompt}{}",
            crate::context::selective::build_file_context(&files)
        )
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
        self.compact_with(CompactionTrigger::Manual).await
    }

    /// Hand live state (project root, todos) to the compaction manager so the
    /// summary can carry it across the cut.
    fn sync_compaction_state(&mut self) {
        let todos = self.get_todo_list().map(|list| {
            list.list()
                .iter()
                .map(|t| format!("- [{}] {}", if t.done { "x" } else { " " }, t.title))
                .collect::<Vec<_>>()
                .join("\n")
        });
        let root = self.project_root.clone();
        if let Some(cm) = &mut self.compaction_manager {
            cm.set_project_root(root);
            cm.set_todos(todos);
        }
    }

    /// Force compaction now with the given trigger and emit the event.
    async fn compact_with(
        &mut self,
        trigger: CompactionTrigger,
    ) -> Result<Option<crate::context::compaction::CompactionResult>, ReactError> {
        self.pending_compaction = None;
        self.sync_compaction_state();
        let Some(compaction_manager) = &mut self.compaction_manager else {
            return Ok(None);
        };
        let summarizer: &dyn Model = match &self.summarizer {
            Some(s) => &**s,
            None => &*self.model,
        };
        let result = compaction_manager
            .compact_now(summarizer, &mut self.conversation, trigger)
            .await
            .map_err(|e| ReactError::Other(anyhow::anyhow!(e)))?;
        if let Some(result) = &result {
            self.emit_compaction_result(trigger, result).await;
        }
        Ok(result)
    }

    async fn emit_compaction_result(
        &mut self,
        trigger: CompactionTrigger,
        result: &crate::context::compaction::CompactionResult,
    ) {
        if let Some(w) = &result.warning {
            self.emit_log(LogLevel::Warn, format!("Compaction: {w}"))
                .await;
        }
        let compaction_event = CompactionEvent {
            trigger,
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

    /// Start summarizing in the background when the context is close to the
    /// trigger, so the summary is ready by the time it is needed.
    fn maybe_start_background_compaction(&mut self) {
        if self.pending_compaction.is_some() {
            return;
        }
        let Some(cm) = &self.compaction_manager else {
            return;
        };
        let Some(plan) = cm.plan_background(&self.conversation, self.model.model_id()) else {
            return;
        };
        let model: Arc<dyn Model> = match &self.summarizer {
            Some(s) => Arc::clone(s),
            None => Arc::clone(&self.model) as Arc<dyn Model>,
        };
        let task_plan = plan.clone();
        let handle =
            tokio::spawn(async move { crate::context::summarize_plan(&*model, &task_plan).await });
        self.pending_compaction = Some(PendingCompaction { plan, handle });
    }

    /// Compact if needed: prefer a finished background summary, then fall back
    /// to compacting synchronously. Failures are logged, not fatal; the hard
    /// cap and overflow recovery remain as backstops.
    async fn auto_compact(&mut self) {
        self.sync_compaction_state();
        let Some(cm) = &self.compaction_manager else {
            return;
        };
        let trigger_tokens = cm.trigger_tokens(&self.conversation);
        let over = self.conversation.estimated_tokens() >= trigger_tokens;
        if !over {
            return;
        }

        if let Some(PendingCompaction { plan, handle }) = self.pending_compaction.take() {
            let applied = match handle.await {
                Ok(Ok(body)) => self.compaction_manager.as_mut().map(|cm| {
                    cm.apply_plan(
                        &mut self.conversation,
                        plan,
                        body,
                        crate::events::CompactionStrategy::Agentic,
                        CompactionTrigger::Auto,
                    )
                }),
                _ => None,
            };
            if let Some(Ok(result)) = applied {
                self.emit_compaction_result(CompactionTrigger::Auto, &result)
                    .await;
                return;
            }
            // Stale or failed: fall through to the synchronous path.
        }

        let Some(cm) = &mut self.compaction_manager else {
            return;
        };
        let summarizer: &dyn Model = match &self.summarizer {
            Some(s) => &**s,
            None => &*self.model,
        };
        match cm
            .maybe_compact(summarizer, &mut self.conversation, CompactionTrigger::Auto)
            .await
        {
            Ok(Some(result)) => {
                self.emit_compaction_result(CompactionTrigger::Auto, &result)
                    .await;
            }
            Ok(None) => {}
            Err(e) => {
                self.emit_log(LogLevel::Warn, format!("Auto-compaction failed: {e}"))
                    .await;
            }
        }
    }

    /// Add a tool result to the conversation. If it does not fit under the
    /// hard cap, compact and retry, then truncate as a last resort.
    async fn add_tool_result_message(
        &mut self,
        output: String,
        id: String,
    ) -> Result<(), ReactError> {
        let Err(first) = self
            .conversation
            .add_tool_message(output.clone(), id.clone())
        else {
            return Ok(());
        };
        if self.compaction_manager.is_some() {
            let _ = self.compact_with(CompactionTrigger::OverflowRecovery).await;
            if self
                .conversation
                .add_tool_message(output.clone(), id.clone())
                .is_ok()
            {
                return Ok(());
            }
        }
        // Keep roughly half of the remaining room (4 bytes per token).
        let keep = (self.conversation.remaining_tokens() * 2).min(output.len());
        if keep < 200 {
            return Err(first.into());
        }
        let (clipped, _) = crate::tool::builtin::truncate_output(&output, keep);
        self.conversation.add_tool_message(clipped, id)?;
        Ok(())
    }

    /// Run the ReAct loop until completion or termination.
    pub async fn run(&mut self, initial_prompt: String) -> Result<String, ReactError> {
        // Expand @file mentions (and optional scan) into the user message
        let initial_prompt = self.expand_prompt(&initial_prompt).await;
        self.conversation
            .add_user_message(initial_prompt)
            .context("failed to add initial prompt")?;

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
        let _signal_guard = AbortOnDrop(tokio::spawn(async move {
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
        }));

        loop {
            // Check for completed sub-agents
            self.check_subagent_completion().await?;

            // Check shutdown signal
            if shutdown_rx.try_recv().is_ok() {
                self.emit_interrupted(false).await; // Signal-based (SIGINT)
                return Err(ReactError::Interrupted);
            }

            // Check iteration/time limits, warn on context, compact if needed
            self.prepare_iteration().await?;

            // Run one iteration
            match self.iterate(&mut shutdown_rx).await? {
                LoopDecision::Continue => {
                    self.iteration += 1;
                    // Take snapshot at loop boundary
                    self.take_boundary_snapshot().await?;
                    self.maybe_start_background_compaction();
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

    /// Run one user turn: iterate think → tool → observe until the model
    /// completes, stops, or a limit is hit.
    ///
    /// This is the entry point used by the TUI in interactive mode; the TUI
    /// owns the input loop and calls this once per user prompt. Iteration and
    /// elapsed-time limits are reset at the start of every turn.
    ///
    /// Unlike `run()`, this method does NOT set up signal handlers or take a
    /// session-start snapshot. The caller provides a shutdown receiver to
    /// allow interrupting the model generation.
    pub async fn run_turn(
        &mut self,
        prompt: String,
        shutdown_rx: mpsc::Receiver<()>,
    ) -> Result<LoopDecision, ReactError> {
        // Expand @file mentions (and optional scan) into the user message
        let prompt = self.expand_prompt(&prompt).await;
        self.conversation
            .add_user_message(prompt)
            .context("failed to add user prompt")?;

        // Emit status update
        self.emit_status().await;

        // Limits apply per user turn, not per session.
        self.iteration = 0;
        self.start_time = Instant::now();
        self.last_call_sig = None;
        self.repeat_count = 0;

        let mut shutdown_rx = shutdown_rx;
        loop {
            self.prepare_iteration().await?;

            match self.iterate(&mut shutdown_rx).await? {
                LoopDecision::Continue => {
                    self.iteration += 1;
                    self.take_boundary_snapshot().await?;
                    self.maybe_start_background_compaction();
                }
                decision @ LoopDecision::Complete(_) => {
                    self.take_snapshot("Session complete").await?;
                    self.maybe_start_background_compaction();
                    return Ok(decision);
                }
                decision @ LoopDecision::Stop(_) => return Ok(decision),
            }
        }
    }

    /// Per-iteration preamble shared by `run` and `run_turn`: enforce limits,
    /// warn on context usage and compact if needed.
    async fn prepare_iteration(&mut self) -> Result<(), ReactError> {
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

        if self.conversation.warning_level() != crate::context::WarningLevel::Ok {
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

        self.auto_compact().await;
        Ok(())
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

        // Anchor token estimates on what the provider actually counted.
        if let Some(usage) = &response.usage {
            self.conversation
                .record_usage(usage.prompt_tokens, usage.completion_tokens);
        }

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
                return Ok(self.decide_without_tools(response.content));
            }

            self.run_tool_calls(tool_calls, shutdown_rx).await?;

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

        Ok(self.decide_without_tools(response.content))
    }

    /// Decide what to do when a native-tool-calling model replied without
    /// tool calls. A reply is final unless it was cut off by the token limit.
    fn decide_without_tools(&self, content: String) -> LoopDecision {
        if self.last_truncated {
            LoopDecision::Continue
        } else {
            LoopDecision::Complete(content)
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
                    self.run_tool_calls(tool_calls, shutdown_rx).await?;
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
                    let result = self.execute_tool_or_error(tool_call, shutdown_rx).await?;
                    self.record_tool_result(result).await?;
                }
                Ok(LoopDecision::Continue)
            }
            Err(TextToolError::NoToolCalls) => self.check_completion(content),
            Err(e) => Err(ReactError::Tool(format!("text tool call failed: {e}"))),
        }
    }

    /// A reply without tool calls is final unless it was truncated.
    fn check_completion(&self, content: &str) -> Result<LoopDecision, ReactError> {
        Ok(self.decide_without_tools(content.to_string()))
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
        let mut attempt = 0usize;
        let mut compacted = false;
        let mut messages = messages.to_vec();
        self.last_truncated = false;

        // Pass native tool specs only when the model supports native function calling.
        // Models without native FC use text-based parsing via the system prompt snippet.
        let native_tools = if self.model.capabilities().tool_calling {
            Some(self.tools.specs())
        } else {
            None
        };

        'attempt: loop {
            // Get a streaming response from the model
            let stream_result = tokio::select! {
                biased;
                result = self.model.stream(
                    &messages,
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
                    if self
                        .recover_from_model_error(
                            &error,
                            &mut attempt,
                            &mut compacted,
                            &mut messages,
                            shutdown_rx,
                        )
                        .await?
                    {
                        continue 'attempt;
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
                            tool_calls.get_or_insert_with(Vec::new).extend(tc);
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
                        if chunk.usage.is_some() {
                            usage = chunk.usage;
                        }
                        if chunk.truncated {
                            self.last_truncated = true;
                        }
                        if chunk.finished {
                            finished = true;
                        }
                    }
                    Some(Err(e)) => {
                        // Only safe to restart when no tool call has been
                        // observed: partial text is simply discarded.
                        if tool_calls.is_none()
                            && self
                                .recover_from_model_error(
                                    &e,
                                    &mut attempt,
                                    &mut compacted,
                                    &mut messages,
                                    shutdown_rx,
                                )
                                .await?
                        {
                            continue 'attempt;
                        }
                        return Err(ReactError::Model(e));
                    }
                    None => finished = true,
                }
            }

            // Providers that don't report usage fall back to an estimate.
            let usage = usage.unwrap_or_else(|| {
                let counter = crate::context::tokens::TokenCounter;
                let prompt_tokens: usize = messages.iter().map(|m| counter.count(&m.content)).sum();
                let completion_tokens = counter.count(&content);
                crate::model::Usage {
                    prompt_tokens,
                    completion_tokens,
                    total_tokens: prompt_tokens + completion_tokens,
                }
            });

            return Ok(ModelResponse {
                content,
                tool_calls,
                thinking,
                usage: Some(usage),
            });
        }
    }

    /// Decide whether a failed model call can be retried, performing the
    /// recovery step (interruptible backoff, or emergency compaction on
    /// context overflow). Returns `true` if the caller should retry.
    async fn recover_from_model_error(
        &mut self,
        error: &ModelError,
        attempt: &mut usize,
        compacted: &mut bool,
        messages: &mut Vec<ModelMessage>,
        shutdown_rx: &mut mpsc::Receiver<()>,
    ) -> Result<bool, ReactError> {
        if matches!(error, ModelError::ContextLengthExceeded(_)) {
            if *compacted || self.compaction_manager.is_none() {
                return Ok(false);
            }
            *compacted = true;
            self.emit_log(
                LogLevel::Warn,
                format!("Context length exceeded; compacting and retrying: {error}"),
            )
            .await;
            if matches!(
                self.compact_with(CompactionTrigger::OverflowRecovery).await,
                Ok(Some(_))
            ) {
                *messages = self.build_model_messages();
                return Ok(true);
            }
            return Ok(false);
        }

        let retry_policy = self.config.retry_policy.clone();
        if retry_policy.classify(error) != RetryDecision::Retry
            || *attempt >= retry_policy.max_retries
        {
            return Ok(false);
        }

        self.retries += 1;
        let backoff = retry_policy.backoff_for_attempt(*attempt, Some(error));
        self.emit_log(
            LogLevel::Warn,
            format!(
                "Provider error (attempt {}/{}), retrying in {:?}: {}",
                *attempt + 1,
                retry_policy.max_retries,
                backoff,
                error
            ),
        )
        .await;

        tokio::select! {
            biased;
            _ = tokio::time::sleep(backoff) => {}
            _ = shutdown_rx.recv() => {
                self.emit_interrupted(false).await;
                return Err(ReactError::Interrupted);
            }
        }
        *attempt += 1;
        Ok(true)
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
        if let Some(result) = self.cached_tool_result(&tool_call).await {
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
        match self
            .policy
            .check_call(&tool_call.name, &spec, &tool_call.args)
        {
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
                        self.policy
                            .allow_always_call(&tool_call.name, &tool_call.args);
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

        // Emit tool call started event.
        self.emit_tool_call(ToolCall {
            id: tool_call.id.clone(),
            name: tool_call.name.clone(),
            args: tool_call.args.clone(),
            status: ToolCallStatus::Pending,
        })
        .await;

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
        Ok(self
            .finish_tool_execution(tool_call, &spec, exec_result, duration_ms)
            .await)
    }

    /// Return a cached result for an idempotent tool call, recording cost and
    /// emitting the completion event as if it had executed.
    async fn cached_tool_result(&mut self, tool_call: &ToolCall) -> Option<ToolResult> {
        let cached = self
            .tool_cache
            .as_mut()?
            .get(&tool_call.name, &tool_call.args)?;

        self.emit_log(
            LogLevel::Debug,
            format!(
                "Tool result cache hit for {} (skipping execution)",
                tool_call.name
            ),
        )
        .await;

        let result = tool_result_from_exec(tool_call, cached, 0);
        self.record_tool_cost(&result).await;
        self.emit_tool_call(ToolCall {
            id: result.id.clone(),
            name: result.name.clone(),
            args: result.args.clone(),
            status: ToolCallStatus::Ok,
        })
        .await;
        Some(result)
    }

    /// Post-process a successful tool execution: cache bookkeeping, cost
    /// recording and the completion event.
    async fn finish_tool_execution(
        &mut self,
        tool_call: ToolCall,
        spec: &crate::tool::tool_trait::ToolSpec,
        exec_result: crate::tool::tool_trait::ToolExecutionResult,
        duration_ms: u64,
    ) -> ToolResult {
        let result = tool_result_from_exec(&tool_call, exec_result.clone(), duration_ms);

        // Store result in cache for idempotent tools (read, grep).
        if let Some(cache) = &mut self.tool_cache {
            cache.put(&tool_call.name, &tool_call.args, &exec_result);
        }

        // Invalidate cache for paths affected by write/delete/run operations.
        if let Some(cache) = &mut self.tool_cache
            && matches!(
                spec.category,
                ToolCategory::Write | ToolCategory::Delete | ToolCategory::Network
            )
        {
            use crate::tool::cache::extract_paths;
            for path in extract_paths(&tool_call.name, &tool_call.args) {
                cache.invalidate_path(&path);
            }
            // `run` can modify arbitrary files.
            if tool_call.name == "run" {
                cache.invalidate_all();
            }
        }

        self.record_tool_cost(&result).await;
        self.emit_tool_call(ToolCall {
            id: result.id.clone(),
            name: result.name.clone(),
            args: result.args.clone(),
            status: ToolCallStatus::Ok,
        })
        .await;

        result
    }

    /// Execute a tool call, converting recoverable failures (unknown tool,
    /// permission denied, execution error) into an error tool result so the
    /// model can see and correct them. Interrupts, budget and approval-channel
    /// failures remain fatal.
    async fn execute_tool_or_error(
        &mut self,
        tool_call: ToolCall,
        shutdown_rx: &mut mpsc::Receiver<()>,
    ) -> Result<ToolResult, ReactError> {
        if let Some(result) = self.check_doom_loop(&tool_call).await? {
            return Ok(result);
        }
        match self.execute_tool(tool_call.clone(), shutdown_rx).await {
            Err(ReactError::Tool(msg)) => Ok(self.error_tool_result(&tool_call, msg).await),
            other => other,
        }
    }

    /// Build an error tool result and emit the Error status event.
    async fn error_tool_result(&mut self, tool_call: &ToolCall, message: String) -> ToolResult {
        self.emit_tool_call(ToolCall {
            id: tool_call.id.clone(),
            name: tool_call.name.clone(),
            args: tool_call.args.clone(),
            status: ToolCallStatus::Error,
        })
        .await;
        ToolResult {
            id: tool_call.id.clone(),
            name: tool_call.name.clone(),
            args: tool_call.args.clone(),
            output: format!("Error: {message}"),
            ok: false,
            duration_ms: 0,
            truncated: false,
            exit_code: None,
            timed_out: false,
            cancelled: false,
        }
    }

    /// Detect the model repeating an identical call. Returns an error result
    /// on the warning threshold and stops the loop on the hard threshold.
    async fn check_doom_loop(
        &mut self,
        tool_call: &ToolCall,
    ) -> Result<Option<ToolResult>, ReactError> {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        tool_call.name.hash(&mut hasher);
        tool_call.args.to_string().hash(&mut hasher);
        let sig = hasher.finish();

        if self.last_call_sig == Some(sig) {
            self.repeat_count += 1;
        } else {
            self.last_call_sig = Some(sig);
            self.repeat_count = 1;
        }

        if self.repeat_count >= DOOM_LOOP_STOP {
            return Err(ReactError::Stopped {
                reason: format!(
                    "repeated identical `{}` call {} times without progress",
                    tool_call.name, self.repeat_count
                ),
            });
        }
        if self.repeat_count >= DOOM_LOOP_WARN {
            let msg = format!(
                "this exact `{}` call was already made {} times with the same arguments. \
                 Do not repeat it; change your approach or finish.",
                tool_call.name,
                self.repeat_count - 1
            );
            return Ok(Some(self.error_tool_result(tool_call, msg).await));
        }
        Ok(None)
    }

    /// Whether a call is a policy-allowed, read-only tool that can run
    /// concurrently with its neighbours.
    fn is_parallel_safe(&self, tool_call: &ToolCall) -> bool {
        if tool_call.name == "todo" {
            return false;
        }
        let Some(tool) = self.tools.get(&tool_call.name) else {
            return false;
        };
        let spec = tool.spec();
        spec.category == ToolCategory::Read
            && self.policy.check(&tool_call.name, &spec) == PolicyDecision::Allow
    }

    /// Execute all tool calls of an assistant turn in order, running runs of
    /// consecutive read-only calls concurrently. Results are appended to the
    /// conversation in the original call order.
    async fn run_tool_calls(
        &mut self,
        tool_calls: Vec<ToolCall>,
        shutdown_rx: &mut mpsc::Receiver<()>,
    ) -> Result<(), ReactError> {
        let mut i = 0;
        while i < tool_calls.len() {
            let mut j = i;
            while j < tool_calls.len() && self.is_parallel_safe(&tool_calls[j]) {
                j += 1;
            }
            if j - i >= 2 {
                self.execute_parallel_batch(tool_calls[i..j].to_vec(), shutdown_rx)
                    .await?;
                i = j;
            } else {
                let result = self
                    .execute_tool_or_error(tool_calls[i].clone(), shutdown_rx)
                    .await?;
                self.record_tool_result(result).await?;
                i += 1;
            }
        }
        Ok(())
    }

    /// Add a tool result to the conversation and emit its event.
    async fn record_tool_result(&mut self, result: ToolResult) -> Result<(), ReactError> {
        self.add_tool_result_message(result.output.clone(), result.id.clone())
            .await?;
        self.emit_tool_result(result).await;
        Ok(())
    }

    /// Run read-only calls concurrently: sequential prepare (cache, budget,
    /// events), concurrent execution, sequential in-order finalize.
    async fn execute_parallel_batch(
        &mut self,
        calls: Vec<ToolCall>,
        shutdown_rx: &mut mpsc::Receiver<()>,
    ) -> Result<(), ReactError> {
        enum Slot {
            Done(ToolResult),
            Run(ToolCall, Arc<dyn crate::tool::tool_trait::Tool>),
        }

        let mut slots = Vec::with_capacity(calls.len());
        for call in calls {
            if let Some(result) = self.check_doom_loop(&call).await? {
                slots.push(Slot::Done(result));
                continue;
            }
            if let Some(result) = self.cached_tool_result(&call).await {
                slots.push(Slot::Done(result));
                continue;
            }
            self.check_expensive_call_confirmation(&call).await?;
            self.check_cost_budget(&call).await?;
            let tool = self
                .tools
                .get(&call.name)
                .expect("checked by is_parallel_safe");
            self.emit_tool_call(ToolCall {
                id: call.id.clone(),
                name: call.name.clone(),
                args: call.args.clone(),
                status: ToolCallStatus::Pending,
            })
            .await;
            slots.push(Slot::Run(call, tool));
        }

        let start = Instant::now();
        let futures: Vec<_> = slots
            .iter()
            .filter_map(|slot| match slot {
                Slot::Run(call, tool) => Some((tool.clone(), call.args.clone())),
                Slot::Done(_) => None,
            })
            .map(|(tool, args)| async move { tool.execute(args).await })
            .collect();

        let outcomes = tokio::select! {
            biased;
            outcomes = futures::future::join_all(futures) => outcomes,
            _ = shutdown_rx.recv() => return Err(ReactError::Interrupted),
        };
        let duration_ms = start.elapsed().as_millis() as u64;

        let mut outcomes = outcomes.into_iter();
        for slot in slots {
            let result = match slot {
                Slot::Done(result) => result,
                Slot::Run(call, tool) => match outcomes.next().expect("one outcome per run slot") {
                    Ok(exec) => {
                        let spec = tool.spec();
                        self.finish_tool_execution(call, &spec, exec, duration_ms)
                            .await
                    }
                    Err(e) => {
                        self.error_tool_result(&call, format!("tool execution failed: {e}"))
                            .await
                    }
                },
            };
            self.record_tool_result(result).await?;
        }
        Ok(())
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
        if self
            .policy
            .has_session_allow_call(&tool_call.name, &tool_call.args)
        {
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
                self.policy
                    .allow_always_call(&tool_call.name, &tool_call.args);
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
                    self.add_tool_result_message(
                        result.result.output.clone(),
                        result.result.id.clone(),
                    )
                    .await?;

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::cap::ContextCap;
    use crate::model::mock::MockModel;
    use crate::tool::tool_trait::{Tool, ToolError, ToolExecutionResult, ToolSpec};
    use async_trait::async_trait;
    use serde_json::{Value, json};
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct TestTool {
        name: &'static str,
        delay_ms: u64,
        fail: bool,
        active: Arc<AtomicUsize>,
        max_active: Arc<AtomicUsize>,
    }

    impl TestTool {
        fn new(name: &'static str) -> Self {
            Self {
                name,
                delay_ms: 0,
                fail: false,
                active: Arc::new(AtomicUsize::new(0)),
                max_active: Arc::new(AtomicUsize::new(0)),
            }
        }
    }

    #[async_trait]
    impl Tool for TestTool {
        fn spec(&self) -> ToolSpec {
            ToolSpec::new(self.name, "test tool", json!({"type": "object"}))
        }

        async fn execute(&self, _args: Value) -> Result<ToolExecutionResult, ToolError> {
            let now = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_active.fetch_max(now, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(self.delay_ms)).await;
            self.active.fetch_sub(1, Ordering::SeqCst);
            if self.fail {
                return Err(ToolError::ExecutionFailed("boom".into()));
            }
            Ok(ToolExecutionResult {
                output: format!("{} ok", self.name),
                exit_code: None,
                timed_out: false,
                cancelled: false,
                truncated: false,
            })
        }

        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
    }

    fn call(id: &str, name: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: name.into(),
            args: json!({}),
            status: ToolCallStatus::Pending,
        }
    }

    fn make_loop(model: MockModel, tools: ToolRegistry) -> ReactLoop<MockModel> {
        let capabilities = model.capabilities().clone();
        let conversation =
            Conversation::new("system".into(), ContextCap::new(100_000), capabilities);
        let (event_tx, _) = tokio::sync::broadcast::channel(1024);
        let retry = RetryPolicy {
            max_retries: 3,
            base_backoff_ms: 1,
            max_backoff_ms: 2,
        };
        ReactLoop::new(
            Arc::new(model),
            conversation,
            tools,
            ReactConfig::default(),
            event_tx,
        )
        .with_retry_policy(retry)
    }

    async fn run_one(
        loop_: &mut ReactLoop<MockModel>,
        prompt: &str,
    ) -> Result<LoopDecision, ReactError> {
        let (_tx, rx) = mpsc::channel(1);
        loop_.run_turn(prompt.into(), rx).await
    }

    fn tool_messages(loop_: &ReactLoop<MockModel>) -> Vec<String> {
        loop_
            .conversation()
            .messages()
            .iter()
            .filter(|m| m.role == MessageRole::Tool)
            .map(|m| m.content.clone())
            .collect()
    }

    #[tokio::test]
    async fn run_turn_loops_until_model_replies_without_tools() {
        let model = MockModel::frontier("m");
        model.push_responses(vec![
            MockModel::tool_call_response("", vec![call("1", "a")]),
            MockModel::text_response("here is the answer"),
        ]);
        let mut tools = ToolRegistry::new();
        tools.register(TestTool::new("a"));
        let mut loop_ = make_loop(model, tools);

        let decision = run_one(&mut loop_, "go").await.unwrap();
        assert_eq!(
            decision,
            LoopDecision::Complete("here is the answer".into())
        );
        assert_eq!(tool_messages(&loop_), vec!["a ok".to_string()]);
    }

    #[tokio::test]
    async fn tool_failure_is_returned_to_the_model() {
        let model = MockModel::frontier("m");
        model.push_responses(vec![
            MockModel::tool_call_response("", vec![call("1", "bad"), call("2", "missing")]),
            MockModel::text_response("recovered"),
        ]);
        let mut tools = ToolRegistry::new();
        tools.register(TestTool {
            fail: true,
            ..TestTool::new("bad")
        });
        let mut loop_ = make_loop(model, tools);

        let decision = run_one(&mut loop_, "go").await.unwrap();
        assert_eq!(decision, LoopDecision::Complete("recovered".into()));
        let msgs = tool_messages(&loop_);
        assert_eq!(msgs.len(), 2);
        assert!(msgs[0].starts_with("Error:") && msgs[0].contains("boom"));
        assert!(msgs[1].starts_with("Error:") && msgs[1].contains("unknown tool"));
    }

    #[tokio::test]
    async fn read_only_calls_run_concurrently_and_keep_order() {
        let model = MockModel::frontier("m");
        model.push_responses(vec![
            MockModel::tool_call_response("", vec![call("1", "a"), call("2", "b")]),
            MockModel::text_response("done"),
        ]);
        let max_active = Arc::new(AtomicUsize::new(0));
        let active = Arc::new(AtomicUsize::new(0));
        let mut tools = ToolRegistry::new();
        for name in ["a", "b"] {
            tools.register(TestTool {
                delay_ms: 50,
                active: active.clone(),
                max_active: max_active.clone(),
                ..TestTool::new(name)
            });
        }
        let mut loop_ = make_loop(model, tools);

        run_one(&mut loop_, "go").await.unwrap();
        assert_eq!(max_active.load(Ordering::SeqCst), 2);
        assert_eq!(
            tool_messages(&loop_),
            vec!["a ok".to_string(), "b ok".to_string()]
        );
    }

    #[tokio::test]
    async fn repeated_identical_calls_are_warned_then_stopped() {
        let model = MockModel::frontier("m");
        for i in 0..8 {
            model.push_response(MockModel::tool_call_response(
                "",
                vec![call(&i.to_string(), "a")],
            ));
        }
        let mut tools = ToolRegistry::new();
        tools.register(TestTool::new("a"));
        let mut loop_ = make_loop(model, tools);

        let err = run_one(&mut loop_, "go").await.unwrap_err();
        assert!(matches!(err, ReactError::Stopped { .. }), "{err:?}");
        let msgs = tool_messages(&loop_);
        assert_eq!(msgs.len(), 4);
        assert!(msgs[2].starts_with("Error:") && msgs[2].contains("Do not repeat"));
    }

    #[tokio::test]
    async fn transient_errors_are_retried_with_backoff() {
        let model = MockModel::frontier("m");
        model.push_responses(vec![
            MockModel::rate_limit_error(0),
            MockModel::rate_limit_error(0),
            MockModel::text_response("ok"),
        ]);
        let mut loop_ = make_loop(model, ToolRegistry::new());

        let decision = run_one(&mut loop_, "go").await.unwrap();
        assert_eq!(decision, LoopDecision::Complete("ok".into()));
        assert_eq!(loop_.retries, 2);
    }

    #[tokio::test]
    async fn context_overflow_without_compaction_is_fatal() {
        let model = MockModel::frontier("m");
        model.push_response(MockModel::context_length_error("too big"));
        let mut loop_ = make_loop(model, ToolRegistry::new());

        let err = run_one(&mut loop_, "go").await.unwrap_err();
        assert!(matches!(
            err,
            ReactError::Model(ModelError::ContextLengthExceeded(_))
        ));
        assert_eq!(loop_.retries, 0);
    }

    #[tokio::test]
    async fn retry_backoff_is_interruptible() {
        let model = MockModel::frontier("m");
        model.push_response(MockModel::rate_limit_error(30));
        let mut loop_ = make_loop(model, ToolRegistry::new());

        let (tx, rx) = mpsc::channel(1);
        tx.send(()).await.unwrap();
        let started = Instant::now();
        let err = loop_.run_turn("go".into(), rx).await.unwrap_err();
        assert!(matches!(err, ReactError::Interrupted), "{err:?}");
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[tokio::test]
    async fn limits_reset_between_turns() {
        let model = MockModel::frontier("m");
        model.push_responses(vec![
            MockModel::text_response("one"),
            MockModel::text_response("two"),
        ]);
        let mut loop_ = make_loop(model, ToolRegistry::new());
        loop_.config.max_elapsed_sec = 1;

        run_one(&mut loop_, "first").await.unwrap();
        tokio::time::sleep(Duration::from_millis(1100)).await;
        let decision = run_one(&mut loop_, "second").await.unwrap();
        assert_eq!(decision, LoopDecision::Complete("two".into()));
    }
}
