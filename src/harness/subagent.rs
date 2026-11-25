//! Sub-agent spawning and execution.
//!
//! Provides isolated ReAct loop execution for sub-agents with:
//! - Profile-based configuration (model, tools, depth, budget, workspace)
//! - Workspace isolation modes (Inherit, Branch via gix worktree, Share)
//! - Token budget enforcement (child CostTracker)
//! - Approval bubbling to parent event stream
//! - Background async execution with progress events

use crate::config::{AgentProfile, AgentRegistry, WorkspaceMode};
use crate::context::{
    Conversation, ConversationError, NativeCompactionRequest, NativeCompactionResponse,
};
use crate::cost::{CostBudgetConfig, CostBudgetError, CostTracker, budget::PricingProviderExt};
use crate::events::{
    Approval, Event, EventSender, SubAgentComplete, SubAgentError as SubAgentErrorEvent,
    SubAgentProgress, ToolResult,
};
use crate::harness::react::{ReactConfig, ReactError, ReactLoop};
use crate::model::{Model, ModelCapabilities, ModelTier};
use crate::session::SessionManager;
use crate::snapshot::store::{SnapshotError, SnapshotStore};
use crate::tool::{CacheConfig, ToolRegistry};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use thiserror::Error;
use tokio::sync::{mpsc, oneshot};

type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Async function to do the actual spawn work (separate to satisfy Send bounds)
#[allow(clippy::result_large_err)]
async fn spawn_subagent_inner(
    config: SpawnConfig,
    agent_registry: Arc<AgentRegistry>,
    todo_id: String,
    agent_name: String,
    prompt: String,
    depth: u32,
    spawn_budget_remaining: u32,
) -> Result<SubAgentHandle, SubAgentError> {
    // 1. Resolve profile from registry
    let profile = agent_registry
        .get(&agent_name)
        .ok_or_else(|| SubAgentError::ProfileNotFound {
            name: agent_name.clone(),
        })?
        .clone();

    // 2. Validate depth and spawn budget
    if depth >= profile.max_depth {
        return Err(SubAgentError::MaxDepthExceeded {
            current: depth,
            max: profile.max_depth,
        });
    }
    if depth >= config.global_max_depth {
        return Err(SubAgentError::MaxDepthExceeded {
            current: depth,
            max: config.global_max_depth,
        });
    }
    if spawn_budget_remaining == 0 {
        return Err(SubAgentError::SpawnBudgetExhausted {
            remaining: spawn_budget_remaining,
        });
    }

    // Validate profile against tool registry
    profile
        .validate(&config.parent_tools)
        .map_err(|e| SubAgentError::ValidationFailed {
            message: e.to_string(),
        })?;

    // 3. Create isolated Conversation with profile-specific system prompt
    let conversation = create_subagent_conversation(&config, &profile, depth).await?;

    // 4. Filter ToolRegistry: tier_tools ∩ allowlist - denylist
    let tools = create_subagent_tools(&config, &profile)?;

    // 5. Create child CostTracker with token_budget limit
    let cost_tracker = create_subagent_cost_tracker(&config, &profile)?;

    // 6. Create WorkspaceHandle per profile.workspace_mode
    let workspace = create_workspace(&config, &profile).await?;

    // 7. Build ReactLoop with sub-agent's config
    let subagent_loop = build_subagent_loop(
        &config,
        &profile,
        conversation,
        tools,
        cost_tracker,
        workspace.cwd().to_path_buf(),
        depth,
    )?;

    // 8. Create approval channel (mpsc) for parent→sub-agent
    let (approval_tx, approval_rx) = mpsc::channel(32);
    let approval_tx_for_handle = approval_tx.clone(); // Clone for the handle

    // Configure the loop with the approval receiver
    let subagent_loop = subagent_loop.with_approval_receiver(approval_rx);

    // 9. Spawn background task running loop_.run(prompt)
    let subagent_id = format!("subagent-{}-{}", todo_id, uuid::Uuid::new_v4().simple());
    let parent_tx = config.parent_tx.clone();
    let subagent_id_clone = subagent_id.clone();
    let todo_id_for_handle = todo_id.clone(); // Clone for the handle
    let todo_id_for_task = todo_id; // Move for the task

    let (abort_tx, abort_rx) = oneshot::channel();
    let join_handle = tokio::spawn(async move {
        run_subagent_task(RunSubagentTaskArgs {
            subagent_id: subagent_id_clone,
            todo_id: todo_id_for_task,
            loop_: subagent_loop,
            initial_prompt: prompt,
            parent_tx,
            _approval_tx: approval_tx,
            abort_rx,
            depth,
        })
        .await
    });

    // 10. Return SubAgentHandle
    Ok(SubAgentHandle {
        subagent_id,
        todo_id: todo_id_for_handle,
        join_handle,
        abort_tx,
        approval_tx: approval_tx_for_handle,
    })
}

/// Wrapper around `Arc<dyn Model>` that implements `Model` by delegating.
struct DynModel(Arc<dyn Model>);

#[async_trait::async_trait]
impl Model for DynModel {
    fn capabilities(&self) -> &ModelCapabilities {
        self.0.capabilities()
    }

    fn model_id(&self) -> &str {
        self.0.model_id()
    }

    async fn complete(
        &self,
        messages: &[crate::model::ModelMessage],
        params: &crate::model::GenerationParams,
        tools: Option<&[crate::tool::tool_trait::ToolSpec]>,
    ) -> Result<crate::model::ModelResponse, crate::model::ModelError> {
        self.0.complete(messages, params, tools).await
    }

    async fn stream(
        &self,
        messages: &[crate::model::ModelMessage],
        params: &crate::model::GenerationParams,
        tools: Option<&[crate::tool::tool_trait::ToolSpec]>,
    ) -> Result<
        std::pin::Pin<
            Box<
                dyn futures::Stream<
                        Item = Result<crate::model::ModelStreamChunk, crate::model::ModelError>,
                    > + Send
                    + 'static,
            >,
        >,
        crate::model::ModelError,
    > {
        self.0.stream(messages, params, tools).await
    }

    async fn compact_native(
        &self,
        request: NativeCompactionRequest,
    ) -> Result<Option<NativeCompactionResponse>, crate::model::ModelError> {
        self.0.compact_native(request).await
    }
}

/// Errors from sub-agent operations.
#[derive(Debug, Error)]
#[allow(clippy::result_large_err)]
pub enum SubAgentError {
    #[error("agent profile not found: {name}")]
    ProfileNotFound { name: String },

    #[error("maximum delegation depth exceeded: current={current}, max={max}")]
    MaxDepthExceeded { current: u32, max: u32 },

    #[error("spawn budget exhausted: remaining={remaining}")]
    SpawnBudgetExhausted { remaining: u32 },

    #[error("profile validation failed: {message}")]
    ValidationFailed { message: String },

    #[error("workspace creation failed: {message}")]
    WorkspaceCreationFailed { message: String },

    #[error("conversation error: {0}")]
    Conversation(#[from] ConversationError),

    #[error("cost budget error: {0}")]
    CostBudget(#[from] CostBudgetError),

    #[error("react loop error: {0}")]
    ReactLoop(#[from] ReactError),

    #[error("snapshot error: {0}")]
    Snapshot(#[from] SnapshotError),

    #[error("git worktree error: {0}")]
    Worktree(#[from] gix::open::Error),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("sub-agent task failed: {reason}")]
    TaskFailed { reason: String },

    #[error("approval channel closed")]
    ApprovalChannelClosed,

    #[error("model resolution failed: {message}")]
    ModelResolutionFailed { message: String },
}

/// Result of a completed sub-agent execution.
#[derive(Debug, Clone)]
pub struct SubAgentResult {
    /// Final tool result or summary output.
    pub result: ToolResult,
    /// Human-readable summary of what was accomplished.
    pub summary: String,
    /// Total tokens used by the sub-agent.
    pub tokens_used: u64,
}

/// Handle to a spawned sub-agent for monitoring and control.
#[derive(Debug)]
pub struct SubAgentHandle {
    /// Unique sub-agent identifier.
    pub subagent_id: String,
    /// The todo ID that was delegated to this sub-agent.
    pub todo_id: String,
    /// Join handle for the background task.
    pub join_handle: tokio::task::JoinHandle<Result<SubAgentResult, ReactError>>,
    /// Abort sender to cancel the sub-agent.
    pub abort_tx: oneshot::Sender<()>,
    /// Channel to send approval decisions to the sub-agent.
    pub approval_tx: mpsc::Sender<Approval>,
}

impl SubAgentHandle {
    /// Abort the sub-agent.
    pub fn abort(self) {
        let _ = self.abort_tx.send(());
    }
}

/// Workspace isolation handle for a sub-agent.
#[derive(Debug)]
pub enum WorkspaceHandle {
    /// Inherit parent's working directory (shared).
    Inherit { cwd: PathBuf },
    /// Git worktree for isolated changes (created on spawn, cleaned up on completion).
    Branch {
        worktree_path: PathBuf,
        repo: Box<gix::Repository>,
    },
    /// Shared directory with path-scoped access (not fully implemented).
    Share { shared_dir: PathBuf },
}

impl WorkspaceHandle {
    /// Get the current working directory for this workspace.
    pub fn cwd(&self) -> &Path {
        match self {
            WorkspaceHandle::Inherit { cwd } => cwd,
            WorkspaceHandle::Branch { worktree_path, .. } => worktree_path,
            WorkspaceHandle::Share { shared_dir } => shared_dir,
        }
    }

    /// Clean up workspace resources (removes worktree for Branch mode).
    pub async fn cleanup(self) -> Result<(), std::io::Error> {
        match self {
            WorkspaceHandle::Inherit { .. } => Ok(()),
            WorkspaceHandle::Share { .. } => Ok(()),
            WorkspaceHandle::Branch { worktree_path, .. } => {
                // Remove the worktree using git CLI
                use std::process::Command;

                let parent_dir = worktree_path.parent().ok_or_else(|| {
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "worktree path has no parent",
                    )
                })?;

                // First, remove the worktree
                let output = Command::new("git")
                    .args(["worktree", "remove", "--force"])
                    .arg(&worktree_path)
                    .current_dir(parent_dir)
                    .output()
                    .map_err(std::io::Error::other)?;

                if !output.status.success() {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    // Worktree might already be removed, check if path still exists
                    if worktree_path.exists() {
                        return Err(std::io::Error::other(format!(
                            "git worktree remove failed: {}",
                            stderr
                        )));
                    }
                }

                // Also prune worktree administrative files
                let _ = Command::new("git")
                    .args(["worktree", "prune"])
                    .current_dir(parent_dir)
                    .output();

                Ok(())
            }
        }
    }
}

/// Configuration for spawning a sub-agent.
pub struct SpawnConfig {
    /// Parent's event sender for bubbling events.
    pub parent_tx: EventSender,
    /// Parent's tool registry (for filtering).
    pub parent_tools: Arc<ToolRegistry>,
    /// Parent's model (for "inherit" model resolution).
    pub parent_model: Arc<dyn Model>,
    /// Parent's model capabilities.
    pub parent_capabilities: ModelCapabilities,
    /// Parent's ReactConfig (for defaults).
    pub parent_config: ReactConfig,
    /// Parent's cost tracker (for child budget derivation).
    pub parent_cost_tracker: Option<CostTracker>,
    /// Parent's project root for selective context.
    pub project_root: Option<PathBuf>,
    /// Parent's session manager (optional).
    pub session: Option<Arc<Mutex<SessionManager>>>,
    /// Parent's snapshot store (optional).
    pub snapshot_store: Option<Arc<Mutex<SnapshotStore>>>,
    /// Parent's compaction config (optional).
    pub compaction_config: Option<crate::context::compaction::CompactionConfig>,
    /// Parent's tool cache config (optional).
    pub tool_cache_config: Option<CacheConfig>,
    /// Maximum sub-agent delegation depth allowed globally.
    pub global_max_depth: u32,
}

impl SpawnConfig {
    /// Get the tier tool subset for the parent's model tier.
    fn tier_tools(&self) -> Vec<String> {
        use crate::context::tier::tool_subsets;
        match self.parent_capabilities.tier {
            ModelTier::Small => tool_subsets::small().into_iter().collect(),
            ModelTier::Mid => tool_subsets::mid().into_iter().collect(),
            ModelTier::Frontier => tool_subsets::frontier().into_iter().collect(),
        }
    }
}

/// Spawn a new sub-agent as a background task.
///
/// # Arguments
/// * `parent` - The parent ReactLoop (provides model, tools, config, event stream)
/// * `agent_registry` - Registry of agent profiles
/// * `todo_id` - ID of the todo item delegating to this sub-agent
/// * `agent_name` - Name of the agent profile to use
/// * `prompt` - Initial prompt for the sub-agent
/// * `depth` - Current delegation depth (0 = first level)
/// * `spawn_budget_remaining` - Remaining spawn budget for this delegation chain
///
/// # Returns
/// A `SubAgentHandle` for monitoring and controlling the sub-agent.
#[allow(clippy::result_large_err)]
pub async fn spawn_subagent<M: Model + 'static>(
    parent: &ReactLoop<M>,
    agent_registry: Arc<AgentRegistry>,
    todo_id: String,
    agent_name: String,
    prompt: String,
    depth: u32,
    spawn_budget_remaining: u32,
) -> Result<SubAgentHandle, SubAgentError> {
    // Build SpawnConfig from parent using accessor methods
    let spawn_config = SpawnConfig {
        parent_tx: parent.event_tx(),
        parent_tools: Arc::new(parent.tools().clone()),
        parent_model: parent.model().clone(),
        parent_capabilities: parent.model_capabilities().clone(),
        parent_config: parent.config().clone(),
        parent_cost_tracker: parent.cost_tracker().cloned(),
        project_root: parent.project_root().cloned(),
        session: parent.session(),
        snapshot_store: parent.snapshot_store(),
        compaction_config: parent.compaction_config(),
        tool_cache_config: parent.tool_cache_config(),
        global_max_depth: 10, // Hard limit per architecture
    };

    spawn_subagent_with_config(
        spawn_config,
        agent_registry,
        todo_id,
        agent_name,
        prompt,
        depth,
        spawn_budget_remaining,
    )
    .await
}

/// Internal spawn function with explicit config (for testing and flexibility).
fn spawn_subagent_with_config(
    config: SpawnConfig,
    agent_registry: Arc<AgentRegistry>,
    todo_id: String,
    agent_name: String,
    prompt: String,
    depth: u32,
    spawn_budget_remaining: u32,
) -> BoxFuture<'static, Result<SubAgentHandle, SubAgentError>> {
    Box::pin(spawn_subagent_inner(
        config,
        agent_registry,
        todo_id,
        agent_name,
        prompt,
        depth,
        spawn_budget_remaining,
    ))
}

/// Build the system prompt for a sub-agent.
fn build_subagent_system_prompt(
    profile: &AgentProfile,
    depth: u32,
    model_id: &str,
    tier: ModelTier,
) -> String {
    let base_prompt = crate::model::model_trait::system_prompt_template(tier);

    let mut prompt = String::new();
    prompt.push_str(base_prompt);
    prompt.push_str("\n\n");
    prompt.push_str(&format!(
        "=== SUB-AGENT CONTEXT ===\n\
        You are a sub-agent (depth {}) with the profile '{}'.\n\
        Description: {}\n\
        Model: {}\n\
        Token budget: {} tokens\n\
        Allowed tools: {}\n\
        Disallowed tools: {}\n\
        Workspace mode: {:?}\n\
        =========================\n",
        depth,
        profile.name,
        profile.description,
        model_id,
        profile.token_budget,
        if profile.tools.is_empty() {
            "all (tier default)".to_string()
        } else {
            profile.tools.join(", ")
        },
        if profile.disallowed_tools.is_empty() {
            "none".to_string()
        } else {
            profile.disallowed_tools.join(", ")
        },
        profile.workspace_mode
    ));

    prompt
}

/// Create a conversation for the sub-agent with profile-specific system prompt.
#[allow(clippy::result_large_err)]
async fn create_subagent_conversation(
    config: &SpawnConfig,
    profile: &AgentProfile,
    depth: u32,
) -> Result<Conversation, SubAgentError> {
    // Determine model ID: "inherit" means use parent's model
    let model_id = if profile.model == "inherit" {
        config.parent_model.model_id().to_string()
    } else {
        profile.model.clone()
    };

    // Determine model tier from parent capabilities or profile
    let tier = if profile.model == "inherit" {
        config.parent_capabilities.tier
    } else {
        // For explicit model, we'd need to look up its tier; default to parent's tier
        config.parent_capabilities.tier
    };

    // Build system prompt with sub-agent context
    let system_prompt = build_subagent_system_prompt(profile, depth, &model_id, tier);

    // Create conversation with parent's context cap (or a reasonable default)
    let max_context = config.parent_capabilities.max_context;
    let capabilities = ModelCapabilities {
        tier,
        max_context,
        ..config.parent_capabilities.clone()
    };

    let conversation = Conversation::with_system_prompt(system_prompt, max_context, capabilities);

    Ok(conversation)
}

/// Create a filtered tool registry for the sub-agent.
#[allow(clippy::result_large_err)]
fn create_subagent_tools(
    config: &SpawnConfig,
    profile: &AgentProfile,
) -> Result<ToolRegistry, SubAgentError> {
    let tier_tools = config.tier_tools();
    let effective_tool_names = profile.effective_tools(&config.parent_tools, &tier_tools);

    // Use the new filter method on ToolRegistry to create a filtered clone
    Ok(config.parent_tools.filter(&effective_tool_names))
}

/// Create a child CostTracker with the profile's token budget.
#[allow(clippy::result_large_err)]
fn create_subagent_cost_tracker(
    config: &SpawnConfig,
    profile: &AgentProfile,
) -> Result<CostTracker, SubAgentError> {
    let parent_tracker =
        config
            .parent_cost_tracker
            .as_ref()
            .ok_or_else(|| SubAgentError::ValidationFailed {
                message: "parent has no cost tracker; cannot create child budget".to_string(),
            })?;

    // Create child budget config with the profile's token budget as session cap
    // Convert token budget to USD estimate using parent's pricing
    let child_config = CostBudgetConfig {
        session_cap_usd: Some(estimate_usd_from_tokens(
            profile.token_budget,
            parent_tracker.model_id(),
            parent_tracker.pricing(),
        )),
        per_turn_warning_usd: parent_tracker.per_turn_warning_usd(),
        expensive_call_confirmation_usd: parent_tracker.expensive_call_confirmation_usd(),
    };

    Ok(CostTracker::new(
        child_config,
        parent_tracker.model_id().to_string(),
        parent_tracker.pricing().clone(),
        parent_tracker.capabilities().clone(),
    ))
}

/// Estimate USD cost from token count using model pricing.
fn estimate_usd_from_tokens(
    tokens: usize,
    model_id: &str,
    pricing: &Arc<dyn crate::context::tokens::PricingProvider + Send + Sync>,
) -> f64 {
    if let Some(p) = pricing.try_pricing(model_id) {
        // Rough estimate: assume 50/50 input/output split
        let input_tokens = tokens / 2;
        let output_tokens = tokens - input_tokens;
        let input_cost = (input_tokens as f64 / 1_000_000.0) * p.input_per_million;
        let output_cost = (output_tokens as f64 / 1_000_000.0) * p.output_per_million;
        input_cost + output_cost
    } else {
        // Fail closed: if pricing unknown, set a conservative cap
        10.0
    }
}

/// Create a workspace handle based on the profile's workspace mode.
#[allow(clippy::result_large_err)]
async fn create_workspace(
    config: &SpawnConfig,
    profile: &AgentProfile,
) -> Result<WorkspaceHandle, SubAgentError> {
    let parent_cwd: PathBuf = config
        .project_root
        .clone()
        .or_else(|| std::env::current_dir().ok())
        .ok_or_else(|| SubAgentError::WorkspaceCreationFailed {
            message: "cannot determine parent working directory".to_string(),
        })?;

    match profile.workspace_mode {
        WorkspaceMode::Inherit => Ok(WorkspaceHandle::Inherit {
            cwd: parent_cwd.to_path_buf(),
        }),
        WorkspaceMode::Branch => {
            // Create a git worktree for isolation using git CLI (gix worktree API is unstable)
            use std::process::Command;

            // Generate a unique worktree name
            let worktree_name = format!("okeanos-subagent-{}", uuid::Uuid::new_v4().simple());
            let worktree_path = parent_cwd.join(&worktree_name);

            // Create the worktree at the current HEAD using git CLI
            let output = Command::new("git")
                .args(["worktree", "add", "--detach"])
                .arg(&worktree_path)
                .arg("HEAD")
                .current_dir(&parent_cwd)
                .output()
                .map_err(SubAgentError::Io)?;

            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                return Err(SubAgentError::WorkspaceCreationFailed {
                    message: format!("git worktree add failed: {}", stderr),
                });
            }

            // Open the repo for cleanup later (clone parent_cwd for gix::open)
            let repo = gix::open(parent_cwd.clone()).map_err(SubAgentError::Worktree)?;
            let repo = Box::new(repo);

            Ok(WorkspaceHandle::Branch {
                worktree_path,
                repo,
            })
        }
        WorkspaceMode::Share => {
            // Shared directory mode - create a subdirectory
            let shared_dir =
                parent_cwd.join(format!(".okeanos/shared-{}", uuid::Uuid::new_v4().simple()));
            std::fs::create_dir_all(&shared_dir).map_err(SubAgentError::Io)?;
            Ok(WorkspaceHandle::Share { shared_dir })
        }
    }
}

/// Build the ReactLoop for the sub-agent.
#[allow(clippy::result_large_err)]
fn build_subagent_loop(
    config: &SpawnConfig,
    profile: &AgentProfile,
    conversation: Conversation,
    tools: ToolRegistry,
    cost_tracker: CostTracker,
    cwd: PathBuf,
    _depth: u32,
) -> Result<ReactLoop<DynModel>, SubAgentError>
where
    SpawnConfig: Send,
{
    // Determine model for sub-agent
    let model: Arc<dyn Model> = if profile.model == "inherit" {
        config.parent_model.clone()
    } else {
        // Would need to resolve explicit model from provider registry
        // For now, fall back to parent model
        config.parent_model.clone()
    };

    // Create ReactConfig with sub-agent specific settings
    let mut subagent_config = config.parent_config.clone();
    // Could adjust max_iterations based on token budget, etc.
    subagent_config.max_iterations = (profile.token_budget / 1000).max(10); // Rough heuristic

    // Wrap model in DynModel to satisfy Model trait bound
    let dyn_model = Arc::new(DynModel(model));

    // Create the ReactLoop
    let loop_ = ReactLoop::new(
        dyn_model,
        conversation,
        tools,
        subagent_config,
        config.parent_tx.clone(),
    )
    .with_project_root(cwd)
    .with_cost_tracker(cost_tracker);

    // Add optional components from parent
    let loop_ = if let Some(session) = &config.session {
        loop_.with_session_arc(session.clone())
    } else {
        loop_
    };

    let loop_ = if let Some(store) = &config.snapshot_store {
        loop_.with_snapshot_store_arc(store.clone())
    } else {
        loop_
    };

    let loop_ = if let Some(compaction_config) = &config.compaction_config {
        loop_.with_compaction(compaction_config.clone())
    } else {
        loop_
    };

    let loop_ = if let Some(cache_config) = &config.tool_cache_config {
        loop_.with_tool_cache(cache_config.clone())
    } else {
        loop_
    };

    // Set policy with sub-agent's allowlist (profile.tools)
    let policy = crate::harness::policy::Policy::new(profile.tools.clone());
    let loop_ = loop_.with_policy(policy);

    Ok(loop_)
}

/// Arguments for running a sub-agent task.
struct RunSubagentTaskArgs {
    subagent_id: String,
    todo_id: String,
    loop_: ReactLoop<DynModel>,
    initial_prompt: String,
    parent_tx: EventSender,
    _approval_tx: mpsc::Sender<Approval>,
    abort_rx: oneshot::Receiver<()>,
    depth: u32,
}

/// Run the sub-agent task in the background.
#[allow(clippy::result_large_err)]
async fn run_subagent_task(args: RunSubagentTaskArgs) -> Result<SubAgentResult, ReactError> {
    let RunSubagentTaskArgs {
        subagent_id,
        todo_id,
        mut loop_,
        initial_prompt,
        parent_tx,
        _approval_tx,
        abort_rx,
        depth,
    } = args;
    // Track progress for periodic updates
    let start_time = Instant::now();
    let initial_prompt_clone = initial_prompt.clone(); // Keep a copy for the result

    // Emit initial progress
    emit_progress(&parent_tx, &subagent_id, 0, "Sub-agent started".to_string()).await;

    // Run the loop with the initial prompt
    let result = tokio::select! {
        biased;
        result = loop_.run(initial_prompt) => result,
        _ = abort_rx => {
            // Aborted by parent
            emit_error(
                &parent_tx,
                &subagent_id,
                "aborted",
                "Sub-agent aborted by parent".to_string(),
            ).await;
            return Err(ReactError::Interrupted);
        }
    };

    match result {
        Ok(final_output) => {
            // Approximate tokens from output length (rough heuristic: ~4 chars per token)
            let total_tokens = (final_output.len() / 4).max(100) as u64;

            // Create summary
            let tool_result = ToolResult {
                id: format!("subagent-result-{}", subagent_id),
                name: "subagent".to_string(),
                args: serde_json::json!({ "prompt": initial_prompt_clone }),
                output: final_output.clone(),
                ok: true,
                duration_ms: start_time.elapsed().as_millis() as u64,
                truncated: false,
                exit_code: Some(0),
                timed_out: false,
                cancelled: false,
            };

            let summary = format!(
                "Sub-agent '{}' completed at depth {} in {:.1}s (~{} tokens)",
                subagent_id,
                depth,
                start_time.elapsed().as_secs_f32(),
                total_tokens
            );

            // Emit completion event
            emit_complete(
                &parent_tx,
                &subagent_id,
                &todo_id,
                &summary,
                total_tokens,
                &tool_result,
            )
            .await;

            Ok(SubAgentResult {
                result: tool_result.clone(),
                summary,
                tokens_used: total_tokens,
            })
        }
        Err(e) => {
            let error_msg = e.to_string();
            emit_error(
                &parent_tx,
                &subagent_id,
                "execution_failed",
                error_msg.clone(),
            )
            .await;
            Err(e)
        }
    }
}

/// Emit a progress event to the parent.
async fn emit_progress(
    parent_tx: &EventSender,
    subagent_id: &str,
    iteration: usize,
    message: String,
) {
    let event = Event::subagent_progress(SubAgentProgress {
        subagent_id: subagent_id.to_string(),
        iteration,
        message,
        ts: current_timestamp(),
    });
    let _ = parent_tx.send(event);
}

/// Emit a completion event to the parent.
async fn emit_complete(
    parent_tx: &EventSender,
    subagent_id: &str,
    todo_id: &str,
    summary: &str,
    tokens_used: u64,
    result: &ToolResult,
) {
    let event = Event::subagent_complete(SubAgentComplete {
        subagent_id: subagent_id.to_string(),
        todo_id: todo_id.to_string(),
        summary: summary.to_string(),
        tokens_used,
        result: result.clone(),
        ts: current_timestamp(),
    });
    let _ = parent_tx.send(event);
}

/// Emit an error event to the parent.
async fn emit_error(parent_tx: &EventSender, subagent_id: &str, code: &str, message: String) {
    let event = Event::subagent_error(SubAgentErrorEvent {
        subagent_id: subagent_id.to_string(),
        code: code.to_string(),
        message,
        ts: current_timestamp(),
    });
    let _ = parent_tx.send(event);
}

/// Get current timestamp in milliseconds.
fn current_timestamp() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
