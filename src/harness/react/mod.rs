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
use tracing::Instrument as _;

use super::gate::GateVerdict;
use super::policy::{Policy, PolicyDecision};

mod authorize;
mod budget;
mod compaction_glue;
mod dispatch;
mod parallel;
mod stall;
mod verify;
use stall::{StallDetector, Verdict};

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
            reduced_tool_set: vec!["read".to_string(), "grep".to_string(), "edit".to_string()],
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
    hooks: Arc<crate::harness::hooks::Hooks>,
    /// Whether `SessionStart` hooks already ran.
    session_started: bool,
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
    /// Prefix of this loop's approval ids (`"<subagent_id>/"` for sub-agents).
    approval_prefix: String,
    /// Builds models for agent profiles that name one explicitly.
    model_resolver: Option<crate::harness::subagent::ModelResolver>,
    /// Caps concurrently running sub-agents.
    agent_slots: Arc<tokio::sync::Semaphore>,
    /// Where approval decisions for sub-agent requests are delivered.
    approval_routes: crate::harness::subagent::ApprovalRoutes,
    /// Whether the approval router task has been installed.
    approval_router_started: bool,
    /// Signature of the previous tool call and its consecutive repeat count.
    stall: StallDetector,
    verify: Option<verify::VerifyConfig>,
    /// Whether the last model response was cut off by the output token limit.
    last_truncated: bool,
    /// Provider-reported prompt/completion tokens summed over the loop's calls.
    tokens_in_total: u64,
    tokens_out_total: u64,
    /// Whether `RunMeta` was logged.
    meta_emitted: bool,
    /// Timing of the last successful model call, consumed by `iterate`.
    last_call: Option<CallTiming>,
}

/// Timing of one successful model attempt.
struct CallTiming {
    attempt: usize,
    ttft_ms: Option<u64>,
    total_ms: u64,
    usage_estimated: bool,
    stop_reason: &'static str,
}

/// Sub-agents allowed to run at the same time.
const MAX_PARALLEL_AGENTS: usize = 4;

/// Messages after the last user message before the task is restated.
const GOAL_REMINDER_GAP: usize = 8;

/// Transient task reminder for small models: the latest user request, restated
/// when it is `GOAL_REMINDER_GAP`+ messages behind and the last message is a tool result.
fn goal_reminder(messages: &[ModelMessage]) -> Option<String> {
    let last_user = messages.iter().rposition(|m| m.role == ModelRole::User)?;
    if messages.last()?.role != ModelRole::Tool || messages.len() - last_user <= GOAL_REMINDER_GAP {
        return None;
    }
    let task: String = messages[last_user].content.chars().take(600).collect();
    Some(format!(
        "[Reminder] The task you are working on: {task}\n\
         Keep going until it is done, then reply with a short summary. Do not repeat steps you already finished."
    ))
}

/// Appended to the system prompt while the permission mode is plan.
const PLAN_MODE_NOTE: &str = "\n\nPLAN MODE ACTIVE: you cannot modify files. Investigate with \
read-only tools (delegate broad searches to the `explore` agent), then call `exit_plan` with \
a concrete step-by-step plan. Do not start implementing until it is approved.";

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
        images: exec.images,
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
            hooks: Arc::new(crate::harness::hooks::Hooks::default()),
            session_started: false,
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
            approval_prefix: String::new(),
            model_resolver: None,
            agent_slots: Arc::new(tokio::sync::Semaphore::new(MAX_PARALLEL_AGENTS)),
            approval_routes: Arc::default(),
            approval_router_started: false,
            stall: StallDetector::default(),
            verify: None,
            last_truncated: false,
            tokens_in_total: 0,
            tokens_out_total: 0,
            meta_emitted: false,
            last_call: None,
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

    /// Get the permission policy.
    pub fn policy(&self) -> &Policy {
        &self.policy
    }

    /// Get the lifecycle hooks.
    pub fn hooks(&self) -> Arc<crate::harness::hooks::Hooks> {
        self.hooks.clone()
    }

    /// Run `SessionStart` (once) and `UserPromptSubmit` hooks for a new user
    /// prompt. Returns the prompt with any hook context appended, or the
    /// reason a hook rejected it.
    async fn begin_prompt(&mut self, prompt: String) -> Result<String, String> {
        if self.hooks.is_empty() {
            return Ok(prompt);
        }
        let mut extra = Vec::new();
        if !self.session_started {
            self.session_started = true;
            extra.extend(self.hooks.session_start().await);
        }
        let outcome = self.hooks.user_prompt_submit(&prompt).await;
        if let Some(reason) = outcome.blocked {
            return Err(reason);
        }
        extra.extend(outcome.context);
        if extra.is_empty() {
            Ok(prompt)
        } else {
            Ok(format!("{prompt}\n\n[hook context]\n{}", extra.join("\n")))
        }
    }

    /// Run `Stop` hooks when the model finishes. Returns feedback to send
    /// back to the model if a hook wants it to keep going; bounded so a hook
    /// cannot loop the agent forever.
    async fn stop_hook_feedback(&mut self, last_message: &str, rounds: &mut u32) -> Option<String> {
        const MAX_STOP_ROUNDS: u32 = 3;
        if self.hooks.is_empty() || *rounds >= MAX_STOP_ROUNDS {
            return None;
        }
        let reason = self.hooks.stop(last_message).await?;
        *rounds += 1;
        Some(format!("[Stop hook] {reason}"))
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

    /// Set the lifecycle hooks run around tool calls.
    pub fn with_hooks(mut self, hooks: Arc<crate::harness::hooks::Hooks>) -> Self {
        self.hooks = hooks;
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
        let config = config.scaled_for_window(self.conversation.cap().max_tokens);
        self.compaction_manager = Some(CompactionManager::new(config));
        self
    }

    /// Run the `[verify]` command after successful edits/writes.
    pub fn with_verify(mut self, section: Option<&crate::config::VerifySection>) -> Self {
        self.verify = verify::VerifyConfig::from_section(section);
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

    /// Set the agent registry for sub-agent delegation. Also registers the
    /// `agent`, `wait_agent` and `exit_plan` tools (the loop handles them).
    pub fn with_agent_registry(mut self, registry: Arc<crate::config::AgentRegistry>) -> Self {
        use crate::tool::agent::{AgentTool, ExitPlanTool, WaitAgentTool};
        let mut names = registry.names();
        names.sort();
        self.tools
            .register(AgentTool::new(registry.describe(), names));
        self.tools.register(WaitAgentTool);
        self.tools.register(ExitPlanTool);
        self.refresh_tool_overhead();
        self.agent_registry = Some(registry);
        self
    }

    fn refresh_tool_overhead(&mut self) {
        let specs = self.tools.specs();
        let overhead = if self.model.capabilities().tool_calling {
            serde_json::to_string(&specs).map_or(0, |s| s.len() / 4)
        } else if specs.is_empty() {
            0
        } else {
            TextToolParser::generate_system_prompt_snippet(&specs).len() / 4
        };
        self.conversation.set_tool_overhead(overhead);
    }

    /// Set how to build models for profiles that name one explicitly.
    pub fn with_model_resolver(
        mut self,
        resolver: crate::harness::subagent::ModelResolver,
    ) -> Self {
        self.model_resolver = Some(resolver);
        self
    }

    /// Model resolver for sub-agent profiles.
    pub fn model_resolver(&self) -> Option<crate::harness::subagent::ModelResolver> {
        self.model_resolver
            .clone()
            .or_else(crate::harness::subagent::global_model_resolver)
    }

    /// Concurrency limiter shared by all sub-agents of this loop.
    pub fn agent_slots(&self) -> Arc<tokio::sync::Semaphore> {
        self.agent_slots.clone()
    }

    /// Set the delegation depth of this loop (children run at parent + 1).
    pub fn with_delegation_depth(mut self, depth: u32) -> Self {
        self.delegation_depth = depth;
        self
    }

    /// Prefix for this loop's approval ids so decisions can be routed back.
    pub fn with_approval_prefix(mut self, prefix: String) -> Self {
        self.approval_prefix = prefix;
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

    /// Add a tool result to the conversation. If it does not fit under the
    /// hard cap, compact and retry, then truncate (dropping images) as a last resort.
    async fn add_tool_result_message(
        &mut self,
        output: String,
        id: String,
        is_error: bool,
        images: Vec<crate::model::ImagePart>,
    ) -> Result<(), ReactError> {
        let Err(first) = self.conversation.add_tool_message_with_images(
            output.clone(),
            id.clone(),
            is_error,
            images.clone(),
        ) else {
            return Ok(());
        };
        if self.compaction_manager.is_some() {
            let _ = self.compact_with(CompactionTrigger::OverflowRecovery).await;
            if self
                .conversation
                .add_tool_message_with_images(output.clone(), id.clone(), is_error, images)
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
        self.conversation
            .add_tool_message_with_status(clipped, id, is_error)?;
        Ok(())
    }

    /// Run the ReAct loop until completion or termination.
    pub async fn run(&mut self, initial_prompt: String) -> Result<String, ReactError> {
        let span = tracing::info_span!("run", model = %self.model.model_id());
        let result = self.run_inner(initial_prompt).instrument(span).await;
        if result.is_err() {
            // Dropping the handles aborts any sub-agent still running.
            self.subagent_handles.clear();
        }
        result
    }

    async fn run_inner(&mut self, initial_prompt: String) -> Result<String, ReactError> {
        let initial_prompt = match self.begin_prompt(initial_prompt).await {
            Ok(prompt) => prompt,
            Err(reason) => {
                return Err(ReactError::Stopped {
                    reason: format!("prompt blocked by hook: {reason}"),
                });
            }
        };
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

        let mut stop_rounds = 0u32;
        loop {
            // Deliver reports of finished background sub-agents
            self.drain_subagents().await?;

            // Check shutdown signal
            if shutdown_rx.try_recv().is_ok() {
                self.emit_interrupted(false).await; // Signal-based (SIGINT)
                return Err(ReactError::Interrupted);
            }

            // Check iteration/time limits, warn on context, compact if needed
            self.prepare_iteration().await?;

            // Run one iteration
            let iter_span = tracing::info_span!("iteration", n = self.iteration);
            match self.iterate(&mut shutdown_rx).instrument(iter_span).await? {
                LoopDecision::Continue => {
                    self.iteration += 1;
                    // Take snapshot at loop boundary
                    self.take_boundary_snapshot().await?;
                    self.maybe_start_background_compaction();
                    continue;
                }
                LoopDecision::Complete(result) => {
                    if self.settle_subagents(&mut shutdown_rx).await? {
                        self.iteration += 1;
                        continue;
                    }
                    if let Some(reason) = self.stop_hook_feedback(&result, &mut stop_rounds).await {
                        self.conversation
                            .add_user_message(reason)
                            .context("failed to add Stop hook feedback")?;
                        self.iteration += 1;
                        continue;
                    }
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
        self.run_turn_with_images(prompt, Vec::new(), shutdown_rx)
            .await
    }

    /// Like [`run_turn`](Self::run_turn), attaching images to the user message.
    pub async fn run_turn_with_images(
        &mut self,
        prompt: String,
        images: Vec<crate::model::ImagePart>,
        shutdown_rx: mpsc::Receiver<()>,
    ) -> Result<LoopDecision, ReactError> {
        let span = tracing::info_span!("turn", model = %self.model.model_id());
        let result = self
            .run_turn_inner(prompt, images, shutdown_rx)
            .instrument(span)
            .await;
        if !matches!(result, Ok(LoopDecision::Complete(_))) {
            // Dropping the handles aborts any sub-agent still running.
            self.subagent_handles.clear();
        }
        result
    }

    async fn run_turn_inner(
        &mut self,
        prompt: String,
        images: Vec<crate::model::ImagePart>,
        shutdown_rx: mpsc::Receiver<()>,
    ) -> Result<LoopDecision, ReactError> {
        let prompt = match self.begin_prompt(prompt).await {
            Ok(prompt) => prompt,
            Err(reason) => {
                return Ok(LoopDecision::Stop(format!(
                    "prompt blocked by hook: {reason}"
                )));
            }
        };
        // Expand @file mentions (and optional scan) into the user message
        let prompt = self.expand_prompt(&prompt).await;
        self.conversation
            .add_user_message_with_images(prompt, images)
            .context("failed to add user prompt")?;

        // Emit status update
        self.emit_status().await;

        // Limits apply per user turn, not per session.
        self.iteration = 0;
        self.start_time = Instant::now();
        self.stall.reset();

        let mut shutdown_rx = shutdown_rx;
        let mut stop_rounds = 0u32;
        loop {
            self.drain_subagents().await?;
            self.prepare_iteration().await?;

            let iter_span = tracing::info_span!("iteration", n = self.iteration);
            match self.iterate(&mut shutdown_rx).instrument(iter_span).await? {
                LoopDecision::Continue => {
                    self.iteration += 1;
                    self.take_boundary_snapshot().await?;
                    self.maybe_start_background_compaction();
                }
                decision @ LoopDecision::Complete(_) => {
                    if self.settle_subagents(&mut shutdown_rx).await? {
                        self.iteration += 1;
                        continue;
                    }
                    if let LoopDecision::Complete(text) = &decision
                        && let Some(reason) = self.stop_hook_feedback(text, &mut stop_rounds).await
                    {
                        self.conversation
                            .add_user_message(reason)
                            .context("failed to add Stop hook feedback")?;
                        self.iteration += 1;
                        continue;
                    }
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
        self.emit_run_meta_once();
        let model_messages = self.build_model_messages();
        let call_span = tracing::info_span!(
            "model_call",
            model = %self.model.model_id(),
            ttft_ms = tracing::field::Empty,
            prompt_tokens = tracing::field::Empty,
            completion_tokens = tracing::field::Empty,
            cache_read_tokens = tracing::field::Empty,
        );
        self.last_call = None;
        let response = self
            .call_model(&model_messages, shutdown_rx)
            .instrument(call_span.clone())
            .await?;

        // Record model usage cost
        let cost_before = self.cost_tracker.as_ref().map(|t| t.spent_usd());
        if let Some(usage) = &response.usage {
            self.record_model_usage(usage).await;
            self.tokens_in_total += usage.prompt_tokens as u64;
            self.tokens_out_total += usage.completion_tokens as u64;
        }
        let call_cost = match (cost_before, self.cost_tracker.as_ref()) {
            (Some(before), Some(t)) => Some(t.spent_usd() - before),
            _ => None,
        };
        if let Some(call) = self.last_call.take() {
            if let Some(u) = &response.usage {
                call_span.record("prompt_tokens", u.prompt_tokens as u64);
                call_span.record("completion_tokens", u.completion_tokens as u64);
                call_span.record("cache_read_tokens", u.cache_read_tokens as u64);
            }
            call_span.record("ttft_ms", call.ttft_ms);
            self.emit_event(Event::ModelCall(crate::events::ModelCall {
                iteration: self.iteration,
                attempt: call.attempt,
                model: self.model.model_id().to_string(),
                ttft_ms: call.ttft_ms,
                total_ms: call.total_ms,
                usage: response.usage.clone(),
                usage_estimated: call.usage_estimated,
                cost_usd: call_cost,
                stop_reason: Some(call.stop_reason.to_string()),
                error: None,
                retried: false,
                ts: current_timestamp(),
            }));
        }

        // Add assistant message to conversation
        self.conversation
            .add_assistant_message_with(
                response.content.clone(),
                response.tool_calls.clone(),
                response.thinking.clone(),
                crate::model::MessageExtras {
                    reasoning: response.reasoning.clone(),
                    ..Default::default()
                },
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
            extras: crate::model::MessageExtras {
                reasoning: response.reasoning.clone(),
                ..Default::default()
            },
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
                    .add_assistant_message_with(
                        simpler_response.content.clone(),
                        simpler_response.tool_calls.clone(),
                        simpler_response.thinking.clone(),
                        crate::model::MessageExtras {
                            reasoning: simpler_response.reasoning.clone(),
                            ..Default::default()
                        },
                    )
                    .context("failed to add fallback assistant message")?;
                self.emit_message(Message {
                    role: MessageRole::Assistant,
                    content: simpler_response.content.clone(),
                    tool_calls: simpler_response.tool_calls.clone(),
                    tool_call_id: None,
                    ts: current_timestamp(),
                    thinking: simpler_response.thinking.clone(),
                    extras: crate::model::MessageExtras {
                        reasoning: simpler_response.reasoning.clone(),
                        ..Default::default()
                    },
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
            ..Default::default()
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
                extras: crate::model::MessageExtras {
                    images: msg.extras.images.clone(),
                    is_error: msg.extras.is_error,
                    ..Default::default()
                },
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
            let attempt_start = Instant::now();
            let attempt_no = attempt;
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
                    let total = attempt_start.elapsed();
                    let retry = self
                        .recover_from_model_error(
                            &error,
                            &mut attempt,
                            &mut compacted,
                            &mut messages,
                            shutdown_rx,
                        )
                        .await?;
                    self.emit_failed_call(attempt_no, total, None, &error, retry);
                    if retry {
                        continue 'attempt;
                    }
                    return Err(ReactError::Model(error));
                }
            };

            // Consume the stream, emitting Token events and building the response
            let mut ttft_ms: Option<u64> = None;
            let mut content = String::new();
            let mut tool_calls: Option<Vec<ToolCall>> = None;
            let mut thinking: Option<String> = None;
            let mut reasoning: Vec<crate::model::ReasoningBlock> = Vec::new();
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
                        if ttft_ms.is_none()
                            && (!chunk.delta.is_empty()
                                || chunk.thinking.is_some()
                                || chunk.tool_calls.is_some())
                        {
                            ttft_ms = Some(attempt_start.elapsed().as_millis() as u64);
                        }
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
                            thinking.get_or_insert_with(String::new).push_str(&th);
                            // Emit thinking event for TUI/headless parity
                            let thinking_event =
                                crate::events::Event::thinking(th, current_timestamp());
                            let _ = self.event_tx.send(thinking_event.clone());
                            if let Some(session) = &self.session {
                                let _ = session.lock().unwrap().log_event(thinking_event);
                            }
                        }
                        reasoning.extend(chunk.reasoning);
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
                        let total = attempt_start.elapsed();
                        let retry = tool_calls.is_none()
                            && self
                                .recover_from_model_error(
                                    &e,
                                    &mut attempt,
                                    &mut compacted,
                                    &mut messages,
                                    shutdown_rx,
                                )
                                .await?;
                        self.emit_failed_call(attempt_no, total, ttft_ms, &e, retry);
                        if retry {
                            continue 'attempt;
                        }
                        return Err(ReactError::Model(e));
                    }
                    None => finished = true,
                }
            }

            // Providers that don't report usage fall back to an estimate.
            let usage_estimated = usage.is_none();
            let usage = usage.unwrap_or_else(|| {
                let counter = crate::context::tokens::TokenCounter;
                let prompt_tokens: usize = messages.iter().map(|m| counter.count(&m.content)).sum();
                let completion_tokens = counter.count(&content);
                crate::model::Usage {
                    prompt_tokens,
                    completion_tokens,
                    total_tokens: prompt_tokens + completion_tokens,
                    ..Default::default()
                }
            });

            self.last_call = Some(CallTiming {
                attempt: attempt_no,
                ttft_ms,
                total_ms: attempt_start.elapsed().as_millis() as u64,
                usage_estimated,
                stop_reason: if self.last_truncated {
                    "length"
                } else if tool_calls.is_some() {
                    "tool_use"
                } else {
                    "end_turn"
                },
            });

            return Ok(ModelResponse {
                content,
                tool_calls,
                thinking,
                reasoning,
                usage: Some(usage),
            });
        }
    }

    /// Log a failed model attempt (before any retry backoff has elapsed).
    fn emit_failed_call(
        &self,
        attempt: usize,
        total: Duration,
        ttft_ms: Option<u64>,
        error: &ModelError,
        retried: bool,
    ) {
        self.emit_event(Event::ModelCall(crate::events::ModelCall {
            iteration: self.iteration,
            attempt,
            model: self.model.model_id().to_string(),
            ttft_ms,
            total_ms: total.as_millis() as u64,
            usage: None,
            usage_estimated: false,
            cost_usd: None,
            stop_reason: None,
            error: Some(error.to_string()),
            retried,
            ts: current_timestamp(),
        }));
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
        self.emit_event(crate::events::Event::interrupted(user_initiated));
    }

    /// Build the model messages, injecting a text-based tool schema into the
    /// system prompt when the model does not support native function calling.
    fn build_model_messages(&self) -> Vec<ModelMessage> {
        let mut messages = self.conversation.to_model_messages();

        if self.policy.mode() == crate::harness::policy::PermissionMode::Plan
            && let Some(system_msg) = messages.first_mut()
        {
            system_msg.content.push_str(PLAN_MODE_NOTE);
        }

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

        // Small models drift on long tool chains: restate the task transiently
        // (never persisted) once it has scrolled far behind the latest results.
        if self.conversation.tier() == crate::model::ModelTier::Small
            && let Some(reminder) = goal_reminder(&messages)
        {
            messages.push(ModelMessage {
                role: crate::model::ModelRole::User,
                content: reminder,
                ..Default::default()
            });
        }

        messages
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

    /// Broadcast an event and append it to the session log (if any).
    fn emit_event(&self, event: Event) {
        let _ = self.event_tx.send(event.clone());
        if let Some(session) = &self.session {
            let _ = session.lock().unwrap().log_event(event);
        }
    }

    /// Emit a message event.
    async fn emit_message(&mut self, msg: Message) {
        self.emit_event(Event::Message(msg));
    }

    /// Emit a tool call event.
    async fn emit_tool_call(&mut self, call: ToolCall) {
        self.emit_event(Event::ToolCall(call));
    }

    /// Emit a live tool output chunk. Ephemeral: broadcast only, never session-logged.
    fn emit_tool_output(&self, call: &ToolCall, seq: u64, chunk: String) {
        let _ = self
            .event_tx
            .send(Event::ToolOutput(crate::events::ToolOutput {
                id: call.id.clone(),
                name: call.name.clone(),
                seq,
                chunk,
                ts: current_timestamp(),
            }));
    }

    /// Emit a tool result event.
    async fn emit_tool_result(&mut self, result: ToolResult) {
        self.emit_event(Event::ToolResult(result));
    }

    /// Record a sub-agent completion in the session log. The event itself was
    /// already sent to the UI by the sub-agent's task.
    fn log_subagent_complete(&self, event: SubAgentComplete) {
        if let Some(session) = &self.session {
            let _ = session
                .lock()
                .unwrap()
                .log_event(Event::SubAgentComplete(event));
        }
    }

    /// Emit a sub-agent error event.
    async fn emit_subagent_error(&mut self, event: SubAgentError) {
        self.emit_event(Event::SubAgentError(event));
    }

    /// Emit a status event.
    async fn emit_status(&mut self) {
        let cost_usd = if let Some(tracker) = &self.cost_tracker {
            tracker.spent_usd()
        } else {
            self.conversation.cost_estimate().total_cost_usd()
        };

        // Provider-reported totals once a call completed; the context
        // estimate before that so the first status isn't empty.
        let tokens_in = if self.tokens_in_total > 0 {
            self.tokens_in_total
        } else {
            self.conversation.estimated_tokens() as u64
        };

        self.emit_event(Event::Status(crate::events::Status {
            model: self.model.model_id().to_string(),
            provider_configured: !self.model.model_id().is_empty(),
            tokens_in,
            tokens_out: self.tokens_out_total,
            cost_usd,
            elapsed_ms: self.start_time.elapsed().as_millis() as u64,
            retries: self.retries,
            mode: self.policy.mode().as_str().to_string(),
        }));
    }

    /// Emit an error event to the event stream.
    async fn emit_error(&mut self, code: &str, message: &str) {
        self.emit_event(Event::error(code.to_string(), message.to_string()));
    }

    /// Emit a log event.
    async fn emit_log(&mut self, level: crate::events::LogLevel, message: String) {
        self.emit_event(Event::Log(crate::events::Log { level, message }));
    }

    /// Log reproducibility metadata once, before the first model request.
    fn emit_run_meta_once(&mut self) {
        if self.meta_emitted {
            return;
        }
        self.meta_emitted = true;
        let sha = |bytes: &[u8]| -> String {
            use sha2::{Digest, Sha256};
            Sha256::digest(bytes)
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect()
        };
        let specs = self.tools.specs();
        let params = &self.config.generation_params;
        let system = self
            .conversation
            .to_model_messages()
            .into_iter()
            .next()
            .map(|m| m.content)
            .unwrap_or_default();
        self.emit_event(Event::RunMeta(crate::events::RunMeta {
            okeanos_version: env!("CARGO_PKG_VERSION").to_string(),
            model: self.model.model_id().to_string(),
            tier: self.model.capabilities().tier.to_string(),
            system_prompt_sha256: sha(system.as_bytes()),
            tools_sha256: sha(serde_json::to_string(&specs).unwrap_or_default().as_bytes()),
            tool_count: specs.len(),
            permission_mode: self.policy.mode().as_str().to_string(),
            temperature: params.temperature,
            max_tokens: params.max_tokens,
            reasoning: params.reasoning.map(|r| format!("{r:?}").to_lowercase()),
            ts: current_timestamp(),
        }));
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

    #[test]
    fn goal_reminder_only_when_task_is_far_behind_a_tool_result() {
        let msg = |role, content: &str| ModelMessage {
            role,
            content: content.to_string(),
            ..Default::default()
        };
        let mut msgs = vec![
            msg(ModelRole::System, "sys"),
            msg(ModelRole::User, "rename foo"),
        ];
        for _ in 0..5 {
            msgs.push(msg(ModelRole::Assistant, "a"));
            msgs.push(msg(ModelRole::Tool, "t"));
        }
        let r = goal_reminder(&msgs).expect("reminder");
        assert!(r.contains("rename foo"));
        msgs.push(msg(ModelRole::Assistant, "done"));
        assert!(goal_reminder(&msgs).is_none());
        assert!(goal_reminder(&msgs[..4]).is_none());
    }
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
                images: Vec::new(),
            })
        }

        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
    }

    struct StreamingTool;

    #[async_trait]
    impl Tool for StreamingTool {
        fn spec(&self) -> ToolSpec {
            ToolSpec::new("stream", "streams", json!({"type": "object"}))
        }

        async fn execute(&self, _args: Value) -> Result<ToolExecutionResult, ToolError> {
            Ok(ToolExecutionResult::output("final"))
        }

        async fn execute_streaming(
            &self,
            _args: Value,
            output: crate::tool::tool_trait::ToolOutputSink,
        ) -> Result<ToolExecutionResult, ToolError> {
            output.send("one\n");
            tokio::task::yield_now().await;
            output.send("two\n");
            Ok(ToolExecutionResult::output("final"))
        }

        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
    }

    #[tokio::test]
    async fn streamed_tool_output_is_emitted_before_the_result() {
        let model = MockModel::frontier("m");
        model.push_responses(vec![
            MockModel::tool_call_response("", vec![call("1", "stream")]),
            MockModel::text_response("done"),
        ]);
        let mut tools = ToolRegistry::new();
        tools.register(StreamingTool);
        let mut loop_ = make_loop(model, tools);
        let mut events = loop_.event_tx().subscribe();

        run_one(&mut loop_, "go").await.unwrap();

        let mut chunks = Vec::new();
        let mut result_seen = false;
        while let Ok(event) = events.try_recv() {
            match event {
                Event::ToolOutput(out) => {
                    assert!(!result_seen, "tool_output after tool_result");
                    assert_eq!(out.id, "1");
                    assert_eq!(out.seq as usize, chunks.len());
                    chunks.push(out.chunk);
                }
                Event::ToolResult(_) => result_seen = true,
                _ => {}
            }
        }
        assert_eq!(chunks, vec!["one\n".to_string(), "two\n".to_string()]);
        assert!(result_seen);
        assert_eq!(tool_messages(&loop_), vec!["final".to_string()]);
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
