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

/// Inputs of one spawn request.
pub struct SpawnRequest {
    pub todo_id: String,
    pub agent_name: String,
    pub prompt: String,
    /// Short label for the fleet view.
    pub description: String,
    pub depth: u32,
    pub spawn_budget_remaining: u32,
}

/// Async function to do the actual spawn work (separate to satisfy Send bounds)
#[allow(clippy::result_large_err)]
async fn spawn_subagent_inner(
    config: SpawnConfig,
    agent_registry: Arc<AgentRegistry>,
    request: SpawnRequest,
) -> Result<SubAgentHandle, SubAgentError> {
    let SpawnRequest {
        todo_id,
        agent_name,
        prompt,
        description,
        depth,
        spawn_budget_remaining,
    } = request;

    // 1. Resolve profile from registry
    let mut profile = agent_registry
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

    // Built-in profiles name tools some tiers lack (e.g. `glob`); drop those
    // instead of failing, but never let an allowlist collapse into "all tools".
    if !profile.tools.is_empty() {
        profile.tools.retain(|t| config.parent_tools.has(t));
        if profile.tools.is_empty() {
            return Err(SubAgentError::ValidationFailed {
                message: format!("none of the tools of agent '{agent_name}' are available"),
            });
        }
    }
    profile
        .disallowed_tools
        .retain(|t| config.parent_tools.has(t));
    profile
        .validate(&config.parent_tools)
        .map_err(|e| SubAgentError::ValidationFailed {
            message: e.to_string(),
        })?;

    // 3. Resolve the model (explicit profile models go through the resolver)
    let model = resolve_model(&config, &profile)?;

    // 4. Create isolated Conversation with profile-specific system prompt
    let conversation = create_subagent_conversation(&profile, depth, &model, &config)?;

    // 5. Filter ToolRegistry: parent tools ∩ allowlist - denylist
    let tools = create_subagent_tools(&config, &profile)?;

    // 6. Create child CostTracker with token_budget limit
    let cost_tracker = create_subagent_cost_tracker(&config, &profile, &model)?;

    // 7. Create WorkspaceHandle per profile.workspace_mode
    let workspace = create_workspace(&config, &profile).await?;

    // 8. Build ReactLoop with sub-agent's config
    let subagent_id = format!("subagent-{}-{}", todo_id, uuid::Uuid::new_v4().simple());
    let subagent_loop = match build_subagent_loop(
        &config,
        &profile,
        model,
        conversation,
        tools,
        cost_tracker,
        workspace.cwd().to_path_buf(),
        depth,
        &subagent_id,
    ) {
        Ok(l) => l,
        Err(e) => {
            let _ = workspace.cleanup().await;
            return Err(e);
        }
    };

    // 9. Create approval channel (mpsc) for parent→sub-agent and register it
    // so the parent's router can deliver decisions for this sub-agent's requests.
    let (approval_tx, approval_rx) = mpsc::channel(32);
    let subagent_loop = if config.approvals_routable {
        config
            .approval_routes
            .lock()
            .unwrap()
            .insert(subagent_id.clone(), approval_tx.clone());
        subagent_loop.with_approval_receiver(approval_rx)
    } else {
        subagent_loop
    };

    // 10. Spawn background task running loop_.run(prompt)
    let (abort_tx, abort_rx) = oneshot::channel();
    let join_handle = tokio::spawn(run_subagent_task(RunSubagentTaskArgs {
        subagent_id: subagent_id.clone(),
        todo_id: todo_id.clone(),
        loop_: subagent_loop,
        initial_prompt: prompt,
        parent_tx: config.parent_tx.clone(),
        abort_rx,
        depth,
        workspace,
        slots: config.slots.clone(),
        approval_routes: config.approval_routes.clone(),
    }));

    // 11. Return SubAgentHandle
    Ok(SubAgentHandle {
        subagent_id,
        todo_id,
        agent: agent_name,
        description,
        join_handle,
        abort_tx,
        approval_tx,
    })
}

/// Pick the sub-agent's model: the parent's for `inherit`, otherwise whatever
/// the configured resolver builds for the profile's model name.
#[allow(clippy::result_large_err)]
fn resolve_model(
    config: &SpawnConfig,
    profile: &AgentProfile,
) -> Result<Arc<dyn Model>, SubAgentError> {
    if profile.model == "inherit" || profile.model == config.parent_model.model_id() {
        return Ok(config.parent_model.clone());
    }
    config
        .model_resolver
        .as_ref()
        .and_then(|resolve| resolve(&profile.model))
        .ok_or_else(|| SubAgentError::ModelResolutionFailed {
            message: format!(
                "agent '{}' wants model '{}', which is not configured",
                profile.name, profile.model
            ),
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
    /// Spend to charge to the parent's budget.
    pub cost_usd: f64,
}

/// Handle to a spawned sub-agent for monitoring and control.
#[derive(Debug)]
pub struct SubAgentHandle {
    /// Unique sub-agent identifier.
    pub subagent_id: String,
    /// The todo ID that was delegated to this sub-agent.
    pub todo_id: String,
    /// Name of the agent profile.
    pub agent: String,
    /// Short label of the delegated task.
    pub description: String,
    /// Join handle for the background task.
    pub join_handle: tokio::task::JoinHandle<Result<SubAgentResult, ReactError>>,
    /// Abort sender to cancel the sub-agent (dropping it aborts too).
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

    /// Finish with the workspace. A `branch` worktree with changes is committed
    /// to a new `okeanos/<name>` branch (kept for the caller to merge) and
    /// removed; an unchanged one is simply removed. Returns a note describing
    /// where the changes went, if there were any.
    pub async fn finalize(self) -> Result<Option<String>, std::io::Error> {
        let WorkspaceHandle::Branch { worktree_path, .. } = &self else {
            return Ok(None);
        };
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(worktree_path)
                .output()
        };
        let dirty = git(&["status", "--porcelain"])?;
        let mut note = None;
        if dirty.status.success() && !dirty.stdout.is_empty() {
            let name = worktree_path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("subagent")
                .replacen("okeanos-subagent-", "okeanos/", 1);
            let committed = git(&["switch", "-c", &name])?.status.success()
                && git(&["add", "-A"])?.status.success()
                && git(&[
                    "-c",
                    "user.name=okeanos",
                    "-c",
                    "user.email=okeanos@localhost",
                    "commit",
                    "-m",
                    "okeanos sub-agent changes",
                ])?
                .status
                .success();
            note = Some(if committed {
                format!(
                    "Changes were committed on branch `{name}`; review with `git diff HEAD...{name}` and merge them if wanted."
                )
            } else {
                format!(
                    "Changes could not be committed automatically and were discarded with {}.",
                    worktree_path.display()
                )
            });
        }
        self.cleanup().await?;
        Ok(note)
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
    /// Parent's permission policy (deny/ask rules and mode are inherited).
    pub parent_policy: crate::harness::policy::Policy,
    /// Parent's hooks (tool events apply to sub-agent calls too).
    pub parent_hooks: Arc<crate::harness::hooks::Hooks>,
    /// Maximum sub-agent delegation depth allowed globally.
    pub global_max_depth: u32,
    /// Builds a model by name for profiles that don't `inherit`.
    pub model_resolver: Option<ModelResolver>,
    /// Limits how many sub-agents run at once; the rest queue for a slot.
    pub slots: Arc<tokio::sync::Semaphore>,
    /// Sub-agent id → channel its approval decisions are routed to.
    pub approval_routes: ApprovalRoutes,
    /// False when the parent can't receive decisions (headless): the
    /// sub-agent then gets no approval receiver, so approval-requiring tool
    /// calls fail fast instead of waiting forever.
    pub approvals_routable: bool,
}

/// Builds a model for a model name (see `AgentProfile::model`).
pub type ModelResolver = Arc<dyn Fn(&str) -> Option<Arc<dyn Model>> + Send + Sync>;

static MODEL_RESOLVER: std::sync::OnceLock<ModelResolver> = std::sync::OnceLock::new();

/// Install the process-wide resolver used when a loop has none of its own.
pub fn set_model_resolver(resolver: ModelResolver) {
    let _ = MODEL_RESOLVER.set(resolver);
}

/// The process-wide model resolver, if installed.
pub fn global_model_resolver() -> Option<ModelResolver> {
    MODEL_RESOLVER.get().cloned()
}

/// Shared table the parent's approval router uses to reach sub-agents.
pub type ApprovalRoutes = Arc<Mutex<std::collections::HashMap<String, mpsc::Sender<Approval>>>>;

/// Tool names that only the top-level agent may use.
fn is_parent_only_tool(name: &str) -> bool {
    crate::tool::agent::is_orchestration_tool(name)
}

impl SpawnConfig {
    /// Tools a sub-agent may draw from: everything the parent has except the
    /// orchestration tools (so sub-agents cannot spawn or exit plan mode).
    fn tier_tools(&self) -> Vec<String> {
        self.parent_tools
            .names()
            .into_iter()
            .filter(|n| !is_parent_only_tool(n))
            .collect()
    }
}

/// Spawn a new sub-agent as a background task.
///
/// # Returns
/// A `SubAgentHandle` for monitoring and controlling the sub-agent.
#[allow(clippy::result_large_err)]
pub async fn spawn_subagent<M: Model + 'static>(
    parent: &ReactLoop<M>,
    agent_registry: Arc<AgentRegistry>,
    request: SpawnRequest,
    approval_routes: ApprovalRoutes,
    approvals_routable: bool,
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
        parent_policy: parent.policy().clone(),
        parent_hooks: parent.hooks(),
        global_max_depth: 10, // Hard limit per architecture
        model_resolver: parent.model_resolver(),
        slots: parent.agent_slots(),
        approval_routes,
        approvals_routable,
    };

    spawn_subagent_with_config(spawn_config, agent_registry, request).await
}

/// Internal spawn function with explicit config (for testing and flexibility).
fn spawn_subagent_with_config(
    config: SpawnConfig,
    agent_registry: Arc<AgentRegistry>,
    request: SpawnRequest,
) -> BoxFuture<'static, Result<SubAgentHandle, SubAgentError>> {
    Box::pin(spawn_subagent_inner(config, agent_registry, request))
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
        You cannot ask the user questions and cannot see the caller's conversation. \
        Your final message is the only thing the caller receives: make it a complete, \
        self-contained report.\n\
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
    if !profile.instructions.is_empty() {
        prompt.push_str(&format!("\n{}\n", profile.instructions));
    }

    prompt
}

/// Create a conversation for the sub-agent with profile-specific system prompt.
#[allow(clippy::result_large_err)]
fn create_subagent_conversation(
    profile: &AgentProfile,
    depth: u32,
    model: &Arc<dyn Model>,
    config: &SpawnConfig,
) -> Result<Conversation, SubAgentError> {
    let capabilities = model.capabilities().clone();
    let tier = capabilities.tier;
    let system_prompt = build_subagent_system_prompt(profile, depth, model.model_id(), tier);

    // Never exceed what the sub-agent's own model can hold.
    let max_context = capabilities
        .max_context
        .min(config.parent_capabilities.max_context.max(1));
    let capabilities = ModelCapabilities {
        max_context,
        ..capabilities
    };

    Ok(Conversation::with_system_prompt(
        system_prompt,
        max_context,
        capabilities,
    ))
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

/// Create the sub-agent's cost tracker. It has no USD cap of its own (the
/// profile's token budget is enforced by the loop); it exists so real usage
/// can be charged to the parent's budget. Absent when the parent doesn't track
/// cost or the model has no known pricing.
#[allow(clippy::result_large_err)]
fn create_subagent_cost_tracker(
    config: &SpawnConfig,
    _profile: &AgentProfile,
    model: &Arc<dyn Model>,
) -> Result<Option<CostTracker>, SubAgentError> {
    let Some(parent_tracker) = config.parent_cost_tracker.as_ref() else {
        return Ok(None);
    };
    if parent_tracker
        .pricing()
        .try_pricing(model.model_id())
        .is_none()
    {
        return Ok(None);
    }
    let child_config = CostBudgetConfig {
        session_cap_usd: None,
        per_turn_warning_usd: None,
        expensive_call_confirmation_usd: None,
    };
    Ok(Some(CostTracker::new(
        child_config,
        model.model_id().to_string(),
        parent_tracker.pricing().clone(),
        model.capabilities().clone(),
    )))
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
#[allow(clippy::result_large_err, clippy::too_many_arguments)]
fn build_subagent_loop(
    config: &SpawnConfig,
    profile: &AgentProfile,
    model: Arc<dyn Model>,
    conversation: Conversation,
    tools: ToolRegistry,
    cost_tracker: Option<CostTracker>,
    cwd: PathBuf,
    depth: u32,
    subagent_id: &str,
) -> Result<ReactLoop<DynModel>, SubAgentError>
where
    SpawnConfig: Send,
{
    // Create ReactConfig with sub-agent specific settings
    let mut subagent_config = config.parent_config.clone();
    subagent_config.max_iterations = profile
        .max_turns
        .unwrap_or_else(|| (profile.token_budget / 1000).max(10));

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
    .with_delegation_depth(depth + 1)
    .with_approval_prefix(format!("{subagent_id}/"));
    let loop_ = match cost_tracker {
        Some(tracker) => loop_.with_cost_tracker(tracker),
        None => loop_,
    };

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

    // Sub-agent allowlist (profile.tools) plus the parent's deny/ask rules and
    // mode; tool hooks apply to its calls as well.
    let policy = config
        .parent_policy
        .derive_for_subagent(profile.tools.clone());
    let loop_ = loop_
        .with_policy(policy)
        .with_hooks(Arc::new(config.parent_hooks.tool_events_only()));

    Ok(loop_)
}

/// Arguments for running a sub-agent task.
struct RunSubagentTaskArgs {
    subagent_id: String,
    todo_id: String,
    loop_: ReactLoop<DynModel>,
    initial_prompt: String,
    parent_tx: EventSender,
    abort_rx: oneshot::Receiver<()>,
    depth: u32,
    workspace: WorkspaceHandle,
    slots: Arc<tokio::sync::Semaphore>,
    approval_routes: ApprovalRoutes,
}

/// Text of the last assistant message, used as a partial result.
fn last_assistant_text(loop_: &ReactLoop<DynModel>) -> String {
    loop_
        .conversation()
        .messages()
        .iter()
        .rev()
        .find(|m| {
            matches!(m.role, crate::events::MessageRole::Assistant) && !m.content.trim().is_empty()
        })
        .map(|m| m.content.clone())
        .unwrap_or_default()
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
        mut abort_rx,
        depth,
        workspace,
        slots,
        approval_routes,
    } = args;

    let outcome = run_subagent_inner(
        &subagent_id,
        &mut loop_,
        initial_prompt,
        &parent_tx,
        &mut abort_rx,
        depth,
        slots,
    )
    .await;

    approval_routes.lock().unwrap().remove(&subagent_id);
    let workspace_note = match workspace.finalize().await {
        Ok(note) => note,
        Err(e) => Some(format!("Workspace cleanup failed: {e}")),
    };

    match outcome {
        Ok(mut result) => {
            if let Some(note) = workspace_note {
                result.result.output.push_str("\n\n");
                result.result.output.push_str(&note);
            }
            emit_complete(
                &parent_tx,
                &subagent_id,
                &todo_id,
                &result.summary,
                result.tokens_used,
                &result.result,
            )
            .await;
            Ok(result)
        }
        Err(e) => {
            let code = if matches!(e, ReactError::Interrupted) {
                "aborted"
            } else {
                "execution_failed"
            };
            emit_error(&parent_tx, &subagent_id, code, e.to_string()).await;
            Err(e)
        }
    }
}

#[allow(clippy::result_large_err)]
async fn run_subagent_inner(
    subagent_id: &str,
    loop_: &mut ReactLoop<DynModel>,
    initial_prompt: String,
    parent_tx: &EventSender,
    abort_rx: &mut oneshot::Receiver<()>,
    depth: u32,
    slots: Arc<tokio::sync::Semaphore>,
) -> Result<SubAgentResult, ReactError> {
    let start_time = Instant::now();
    let prompt_for_result = initial_prompt.clone();

    // Queue for a concurrency slot (abortable).
    let _permit = tokio::select! {
        biased;
        _ = &mut *abort_rx => return Err(ReactError::Interrupted),
        permit = slots.acquire_owned() => permit.ok(),
    };

    emit_progress(parent_tx, subagent_id, 0, "Sub-agent started".to_string()).await;

    let result = tokio::select! {
        biased;
        _ = &mut *abort_rx => return Err(ReactError::Interrupted),
        result = loop_.run(initial_prompt) => result,
    };

    let (tokens_used, cost_usd) = loop_
        .cost_tracker()
        .map(|t| (t.tokens_used(), t.spent_usd()))
        .unwrap_or((0, 0.0));

    let (output, ok, stopped) = match result {
        Ok(out) => (out, true, None),
        Err(
            e @ (ReactError::MaxIterations { .. }
            | ReactError::MaxElapsed { .. }
            | ReactError::CostBudget(_)
            | ReactError::Stopped { .. }),
        ) => {
            let partial = last_assistant_text(loop_);
            let out = format!(
                "Stopped early: {e}.\nPartial output:\n{}",
                if partial.is_empty() {
                    "(none)"
                } else {
                    partial.as_str()
                }
            );
            (out, false, Some(e.to_string()))
        }
        Err(e) => return Err(e),
    };

    let tool_result = ToolResult {
        id: format!("subagent-result-{subagent_id}"),
        name: "subagent".to_string(),
        args: serde_json::json!({ "prompt": prompt_for_result }),
        output,
        ok,
        duration_ms: start_time.elapsed().as_millis() as u64,
        truncated: false,
        exit_code: Some(if ok { 0 } else { 1 }),
        timed_out: false,
        cancelled: false,
    };
    let summary = format!(
        "Sub-agent '{subagent_id}' {} at depth {depth} in {:.1}s ({tokens_used} tokens){}",
        if ok { "completed" } else { "stopped early" },
        start_time.elapsed().as_secs_f32(),
        stopped.map(|s| format!(": {s}")).unwrap_or_default()
    );
    Ok(SubAgentResult {
        result: tool_result,
        summary,
        tokens_used,
        cost_usd,
    })
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
