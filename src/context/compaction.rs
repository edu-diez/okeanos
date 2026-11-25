//! Context compaction for long sessions.
//!
//! Pipeline, cheapest first:
//! 1. **Prune**: clear old tool outputs (no model call, no information about
//!    decisions is lost, only raw output that can be re-fetched).
//! 2. **Native**: provider-side compaction, if the model offers it.
//! 3. **Agentic**: LLM summary that updates the previous summary iteratively.
//! 4. **Basic**: deterministic summary (user requests, files, tool activity).
//!
//! Compaction replaces everything before a cut point with one summary message
//! and keeps the recent tail verbatim. The cut never separates a tool result
//! from the assistant message that requested it.
//!
//! Work is split into `plan` (owned snapshot), `summarize_plan` (async, no
//! borrows on the conversation) and `apply_plan`, so a summary can be
//! computed in the background while the conversation keeps going.

use crate::context::tokens::TokenCounter;
use crate::context::{Conversation, ConversationError};
use crate::events::{CompactionEvent, CompactionStrategy, CompactionTrigger, Message, MessageRole};
use crate::model::{Model, ModelCapabilities, ModelError, ModelMessage, ModelRole, ModelTier};
use crate::session::header::current_timestamp;
use crate::tool::builtin::truncate_output;
use crate::tool::tool_trait::ToolSpec;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use thiserror::Error;

/// Opening tag of the summary message that replaces compacted history.
pub const SUMMARY_OPEN: &str = "<context_summary>";
const SUMMARY_CLOSE: &str = "</context_summary>";
/// Separates the model-written (or deterministic) body from the sections that
/// are regenerated on every compaction (todos, recent files).
const LIVE_STATE_DELIM: &str = "<!-- live-state -->";

const MAX_USER_REQUESTS: usize = 12;
const MAX_REINJECTED_FILE_CHARS: usize = 4000;
const MIN_WINDOW_FOR_REINJECT: usize = 32_768;
const TOOL_RESULT_RENDER_CHARS: usize = 1500;
const MESSAGE_RENDER_CHARS: usize = 6000;
const TOOL_ARGS_RENDER_CHARS: usize = 400;

/// Compaction configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CompactionConfig {
    /// Enable compaction.
    pub enabled: bool,
    /// Compact when context reaches this fraction of the window. When unset,
    /// compaction triggers at `window - reserve_tokens`.
    pub trigger_ratio: Option<f32>,
    /// Tokens kept free for the next response (default: 16384; capped at a
    /// quarter of the window).
    pub reserve_tokens: usize,
    /// Target ratio: compact down to this fraction of the window (default: 0.5).
    pub target_ratio: f32,
    /// Number of recent tokens to preserve verbatim (default: 15000).
    pub preserve_recent_tokens: usize,
    /// Compaction strategy: "basic" | "agentic" | "auto" (tier-aware).
    pub strategy: CompactionStrategyConfig,
    /// Optional cheaper model for agentic summarization.
    pub summarizer_model: Option<String>,
    /// Per-provider native compaction settings.
    pub providers: CompactionProvidersConfig,
    /// Per-model strategy overrides (keyed by model id, falling back to tier name).
    pub model_overrides: HashMap<String, CompactionModelOverride>,
    /// Clear old tool outputs before summarizing.
    pub prune_tool_results: bool,
    /// Most recent tool-output tokens that pruning never touches.
    pub prune_protect_tokens: usize,
    /// Pruning only happens if it frees at least this many tokens.
    pub prune_min_savings_tokens: usize,
    /// Pre-compute agentic summaries in the background.
    pub background: bool,
    /// Number of recently touched files whose current contents are re-injected
    /// after compaction.
    pub recent_file_reinject: usize,
}

impl Default for CompactionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            trigger_ratio: None,
            reserve_tokens: 16_384,
            target_ratio: 0.5,
            preserve_recent_tokens: 15_000,
            strategy: CompactionStrategyConfig::Auto,
            summarizer_model: None,
            providers: CompactionProvidersConfig::default(),
            model_overrides: HashMap::new(),
            prune_tool_results: true,
            prune_protect_tokens: 40_000,
            prune_min_savings_tokens: 10_000,
            background: true,
            recent_file_reinject: 3,
        }
    }
}

impl CompactionConfig {
    /// Build the runtime config from the (optional) `[compaction]` config section.
    pub fn from_section(section: Option<&crate::config::CompactionSection>) -> Self {
        let mut c = Self::default();
        let Some(s) = section else {
            return c;
        };
        if let Some(v) = s.enabled {
            c.enabled = v;
        }
        c.trigger_ratio = s.trigger_ratio;
        if let Some(v) = s.target_ratio {
            c.target_ratio = v;
        }
        if let Some(v) = s.preserve_recent_tokens {
            c.preserve_recent_tokens = v;
        }
        if let Some(v) = s.reserve_tokens {
            c.reserve_tokens = v;
        }
        if let Some(v) = s.strategy.as_deref().and_then(|s| s.parse().ok()) {
            c.strategy = v;
        }
        c.summarizer_model = s.summarizer_model.clone();
        if let Some(v) = s.prune_tool_results {
            c.prune_tool_results = v;
        }
        if let Some(v) = s.prune_protect_tokens {
            c.prune_protect_tokens = v;
        }
        if let Some(v) = s.prune_min_savings_tokens {
            c.prune_min_savings_tokens = v;
        }
        if let Some(v) = s.background {
            c.background = v;
        }
        if let Some(v) = s.providers.openai_responses.native_enabled {
            c.providers.openai_responses.native_enabled = v;
        }
        if let Some(v) = &s.providers.openai_responses.native_version {
            c.providers.openai_responses.native_version = v.clone();
        }
        if let Some(v) = s.providers.anthropic.native_enabled {
            c.providers.anthropic.native_enabled = v;
        }
        if let Some(v) = &s.providers.anthropic.compaction_type {
            c.providers.anthropic.compaction_type = v.clone();
        }
        for (name, o) in &s.model_overrides {
            c.model_overrides.insert(
                name.clone(),
                CompactionModelOverride {
                    strategy: o.strategy.as_deref().and_then(|s| s.parse().ok()),
                    preserve_recent_tokens: o.preserve_recent_tokens,
                },
            );
        }
        c
    }
}

/// Compaction strategy configuration.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum CompactionStrategyConfig {
    /// Deterministic summary: user requests, files, tool activity.
    Basic,
    /// LLM summarization with structured prompt.
    Agentic,
    /// Auto: Basic for Small tier, Agentic for Mid/Frontier.
    #[default]
    Auto,
}

impl std::str::FromStr for CompactionStrategyConfig {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "basic" => Ok(Self::Basic),
            "agentic" => Ok(Self::Agentic),
            "auto" => Ok(Self::Auto),
            _ => Err(format!("invalid compaction strategy: {}", s)),
        }
    }
}

/// Per-provider native compaction settings.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CompactionProvidersConfig {
    /// OpenAI Responses API native compaction.
    pub openai_responses: OpenAIResponsesCompactionConfig,
    /// Anthropic native compaction.
    pub anthropic: AnthropicCompactionConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct OpenAIResponsesCompactionConfig {
    /// Use native compaction endpoint (default: true).
    #[serde(default = "default_true")]
    pub native_enabled: bool,
    /// Native compaction version: "v1" or "v2" (default: "v2").
    #[serde(default = "default_v2")]
    pub native_version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AnthropicCompactionConfig {
    /// Use native compaction API (default: true).
    #[serde(default = "default_true")]
    pub native_enabled: bool,
    /// Compaction type: "on_demand" or "threshold" (default: "on_demand").
    #[serde(default = "default_on_demand")]
    pub compaction_type: String,
}

fn default_true() -> bool {
    true
}
fn default_v2() -> String {
    "v2".to_string()
}
fn default_on_demand() -> String {
    "on_demand".to_string()
}

/// Per-model compaction override.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CompactionModelOverride {
    /// Force strategy for this model.
    pub strategy: Option<CompactionStrategyConfig>,
    /// Override preserve_recent_tokens for this model.
    pub preserve_recent_tokens: Option<usize>,
}

/// Compaction budget for a turn.
#[derive(Debug, Clone)]
pub struct CompactionBudget {
    /// Maximum tokens allowed in the request (context cap).
    pub max_tokens: usize,
    /// Trigger threshold in tokens.
    pub trigger_tokens: usize,
    /// Target tokens after compaction.
    pub target_tokens: usize,
    /// Tokens to preserve in recent conversation (tail).
    pub preserve_recent_tokens: usize,
    /// Recent tool-output tokens that pruning leaves alone.
    pub prune_protect_tokens: usize,
    /// Minimum savings for pruning to be worthwhile.
    pub prune_min_savings: usize,
}

impl CompactionBudget {
    /// Create a budget from conversation cap and config.
    ///
    /// Every figure is clamped against the window so small-context models get
    /// proportionate values instead of the large-model defaults.
    pub fn from_conversation(conv: &Conversation, config: &CompactionConfig) -> Self {
        let max_tokens = conv.cap().max_tokens;
        let reserve = config.reserve_tokens.min(max_tokens / 4);
        let trigger_tokens = match config.trigger_ratio {
            Some(r) => (max_tokens as f32 * r) as usize,
            None => max_tokens - reserve,
        };
        let target_tokens =
            ((max_tokens as f32 * config.target_ratio) as usize).min(trigger_tokens * 9 / 10);
        Self {
            max_tokens,
            trigger_tokens,
            target_tokens,
            preserve_recent_tokens: config.preserve_recent_tokens.min(target_tokens / 2),
            prune_protect_tokens: config.prune_protect_tokens.min(max_tokens / 4),
            prune_min_savings: config.prune_min_savings_tokens.min(max_tokens / 16),
        }
    }

    /// Check if compaction is needed.
    pub fn needs_compaction(&self, current_tokens: usize) -> bool {
        current_tokens >= self.trigger_tokens
    }
}

/// Result of a compaction operation.
#[derive(Debug, Clone)]
pub struct CompactionResult {
    /// Strategy that was used.
    pub strategy_used: CompactionStrategy,
    /// Tokens before compaction.
    pub tokens_before: usize,
    /// Tokens after compaction.
    pub tokens_after: usize,
    /// Number of messages removed.
    pub messages_removed: usize,
    /// Number of turns summarized.
    pub turns_summarized: usize,
    /// Human-readable summary.
    pub summary: String,
    /// Whether native provider compaction was used.
    pub native_provider: Option<String>,
    /// Set when a preferred strategy failed and a fallback was used.
    pub warning: Option<String>,
}

/// Errors from compaction operations.
#[derive(Debug, Error)]
pub enum CompactionError {
    #[error("conversation error: {0}")]
    Conversation(#[from] ConversationError),
    #[error("model error: {0}")]
    Model(#[from] ModelError),
    #[error("summarizer model not configured")]
    SummarizerNotConfigured,
    #[error("compaction failed: {0}")]
    Failed(String),
    #[error("no summarizer available for agentic compaction")]
    NoSummarizer,
}

/// Native compaction request sent to provider.
#[derive(Debug, Clone)]
pub struct NativeCompactionRequest {
    pub messages: Vec<ModelMessage>,
    pub system_prompt: String,
    pub tools: Vec<ToolSpec>,
    pub keep_recent_tokens: usize,
    pub trigger: CompactionTrigger,
}

/// Native compaction response from provider.
#[derive(Debug, Clone)]
pub struct NativeCompactionResponse {
    /// Opaque checkpoint to replay on subsequent requests.
    pub checkpoint: CompactionCheckpoint,
    /// Human-readable summary for logging.
    pub summary_text: String,
    /// Tokens used by compaction request.
    pub tokens_used: usize,
}

/// Provider-specific compaction checkpoint.
#[derive(Debug, Clone)]
pub enum CompactionCheckpoint {
    /// OpenAI Responses v2: encrypted blob + retained user messages.
    OpenAIEncrypted {
        blob: String,
        retained_user_messages: Vec<ModelMessage>,
    },
    /// Anthropic: their compaction block format.
    AnthropicBlock { block: serde_json::Value },
    /// Generic: client-side summary message to inject.
    ClientSummary { summary_message: ModelMessage },
}

/// File operations seen in the conversation, ordered least to most recent.
#[derive(Debug, Clone, Default)]
pub struct FileOperationSummary {
    pub read_files: Vec<String>,
    pub modified_files: Vec<String>,
}

impl FileOperationSummary {
    pub fn merge(&mut self, other: &FileOperationSummary) {
        for f in &other.read_files {
            touch(&mut self.read_files, f);
        }
        for f in &other.modified_files {
            touch(&mut self.modified_files, f);
        }
    }

    pub fn is_empty(&self) -> bool {
        self.read_files.is_empty() && self.modified_files.is_empty()
    }
}

/// Move `path` to the most-recent end of `list`.
fn touch(list: &mut Vec<String>, path: &str) {
    list.retain(|p| p != path);
    list.push(path.to_string());
}

/// Trait for observing compaction events (extension point for M23/M27).
pub trait CompactionObserver: Send + Sync {
    /// Called after compaction completes.
    fn on_compaction(&self, event: &CompactionEvent);

    /// Get a summary of compacted history for a query (M27: sub-agent delegation).
    fn get_compacted_history_summary(&self, query: &str) -> Option<String>;
}

/// No-op observer for default use.
pub struct NoopCompactionObserver;

impl CompactionObserver for NoopCompactionObserver {
    fn on_compaction(&self, _event: &CompactionEvent) {}
    fn get_compacted_history_summary(&self, _query: &str) -> Option<String> {
        None
    }
}

/// Owned snapshot of everything needed to compute a summary without
/// borrowing the conversation.
#[derive(Debug, Clone)]
pub struct CompactionPlan {
    /// Index of the first message that is kept verbatim.
    pub cut_index: usize,
    /// Conversation generation the plan was made against.
    pub generation: u64,
    /// Messages that will be replaced by the summary.
    pub head: Vec<Message>,
    /// Body of the previous summary, if the head starts with one.
    pub previous_summary: Option<String>,
    /// Cumulative file operations including those in `head`.
    pub file_ops: FileOperationSummary,
    /// Cumulative user requests including those in `head`.
    pub user_requests: Vec<String>,
    /// Character budget for the summarizer input.
    pub max_chars: usize,
    /// Output token budget for the summary.
    pub max_output_tokens: usize,
}

/// Main compaction manager.
pub struct CompactionManager {
    config: CompactionConfig,
    token_counter: TokenCounter,
    observers: Vec<Box<dyn CompactionObserver>>,
    /// File operations across all compactions so far.
    file_ops: FileOperationSummary,
    /// User requests across all compactions so far (most recent last).
    user_requests: Vec<String>,
    project_root: Option<PathBuf>,
    todos: Option<String>,
}

impl CompactionManager {
    /// Create a new compaction manager.
    pub fn new(config: CompactionConfig) -> Self {
        Self {
            config,
            token_counter: TokenCounter,
            observers: Vec::new(),
            file_ops: FileOperationSummary::default(),
            user_requests: Vec::new(),
            project_root: None,
            todos: None,
        }
    }

    /// Get the compaction configuration.
    pub fn config(&self) -> &CompactionConfig {
        &self.config
    }

    /// Root used to resolve relative paths when re-injecting recent files.
    pub fn set_project_root(&mut self, root: Option<PathBuf>) {
        self.project_root = root;
    }

    /// Current todo list (pre-formatted) to carry across compaction.
    pub fn set_todos(&mut self, todos: Option<String>) {
        self.todos = todos.filter(|t| !t.trim().is_empty());
    }

    /// Register a compaction observer.
    pub fn register_observer(&mut self, observer: Box<dyn CompactionObserver>) {
        self.observers.push(observer);
    }

    /// Token count at which automatic compaction triggers.
    pub fn trigger_tokens(&self, conversation: &Conversation) -> usize {
        CompactionBudget::from_conversation(conversation, &self.config).trigger_tokens
    }

    /// Resolve the effective strategy: model-id override, then tier override,
    /// then the configured strategy (with `auto` resolved by tier).
    pub fn resolve_strategy(
        &self,
        model_id: &str,
        capabilities: &ModelCapabilities,
    ) -> CompactionStrategyConfig {
        let overridden = self
            .config
            .model_overrides
            .get(model_id)
            .or_else(|| {
                self.config
                    .model_overrides
                    .get(&capabilities.tier.to_string())
            })
            .and_then(|o| o.strategy);
        match overridden.unwrap_or(self.config.strategy) {
            CompactionStrategyConfig::Auto => {
                if capabilities.tier == ModelTier::Small {
                    CompactionStrategyConfig::Basic
                } else {
                    CompactionStrategyConfig::Agentic
                }
            }
            s => s,
        }
    }

    fn budget(&self, conversation: &Conversation, model_id: &str) -> CompactionBudget {
        let mut budget = CompactionBudget::from_conversation(conversation, &self.config);
        let tier = conversation.capabilities().tier.to_string();
        if let Some(p) = self
            .config
            .model_overrides
            .get(model_id)
            .or_else(|| self.config.model_overrides.get(&tier))
            .and_then(|o| o.preserve_recent_tokens)
        {
            budget.preserve_recent_tokens = p.min(budget.target_tokens / 2);
        }
        budget
    }

    /// Compact if the conversation has reached the trigger threshold.
    pub async fn maybe_compact<M: Model + ?Sized>(
        &mut self,
        model: &M,
        conversation: &mut Conversation,
        trigger: CompactionTrigger,
    ) -> Result<Option<CompactionResult>, CompactionError> {
        self.run(model, conversation, trigger, false).await
    }

    /// Compact regardless of the current token count (manual `/compact`,
    /// overflow recovery).
    pub async fn compact_now<M: Model + ?Sized>(
        &mut self,
        model: &M,
        conversation: &mut Conversation,
        trigger: CompactionTrigger,
    ) -> Result<Option<CompactionResult>, CompactionError> {
        self.run(model, conversation, trigger, true).await
    }

    async fn run<M: Model + ?Sized>(
        &mut self,
        model: &M,
        conversation: &mut Conversation,
        trigger: CompactionTrigger,
        force: bool,
    ) -> Result<Option<CompactionResult>, CompactionError> {
        if !self.config.enabled {
            return Ok(None);
        }
        let budget = self.budget(conversation, model.model_id());
        let tokens_before = conversation.estimated_tokens();
        if !force && !budget.needs_compaction(tokens_before) {
            return Ok(None);
        }

        // 1. Prune old tool output.
        let mut prune_result = None;
        if self.config.prune_tool_results
            && let Some(stats) = conversation
                .prune_tool_results(budget.prune_protect_tokens, budget.prune_min_savings)
        {
            let result = CompactionResult {
                strategy_used: CompactionStrategy::Prune,
                tokens_before,
                tokens_after: conversation.estimated_tokens(),
                messages_removed: 0,
                turns_summarized: 0,
                summary: format!(
                    "Cleared {} old tool outputs (~{} tokens)",
                    stats.messages_pruned, stats.tokens_saved
                ),
                native_provider: None,
                warning: None,
            };
            if !force && !budget.needs_compaction(result.tokens_after) {
                self.notify_observers(&result, trigger);
                return Ok(Some(result));
            }
            prune_result = Some(result);
        }

        let Some(plan) = self.plan_with(conversation, &budget) else {
            if let Some(r) = &prune_result {
                self.notify_observers(r, trigger);
            }
            return Ok(prune_result);
        };

        // 2. Provider-native compaction (client-summary checkpoints only).
        if let Some(body) = self
            .try_native_compaction(model, &plan, &budget, trigger)
            .await
        {
            let mut result = self.apply_plan(
                conversation,
                plan,
                body,
                CompactionStrategy::Native,
                trigger,
            )?;
            result.tokens_before = tokens_before;
            result.native_provider = Some(model.model_id().to_string());
            return Ok(Some(result));
        }

        // 3./4. Agentic summary, falling back to deterministic.
        let mut warning = None;
        let (body, strategy) =
            match self.resolve_strategy(model.model_id(), conversation.capabilities()) {
                CompactionStrategyConfig::Basic => (basic_body(&plan), CompactionStrategy::Basic),
                _ => match summarize_plan(model, &plan).await {
                    Ok(body) => (body, CompactionStrategy::Agentic),
                    Err(e) => {
                        warning = Some(format!("agentic summary failed ({e}); used basic summary"));
                        (basic_body(&plan), CompactionStrategy::Basic)
                    }
                },
            };
        let mut result = self.apply_plan(conversation, plan, body, strategy, trigger)?;
        result.tokens_before = tokens_before;
        result.warning = warning;
        Ok(Some(result))
    }

    /// Plan a compaction for the current conversation, if there is anything to compact.
    pub fn plan(&self, conversation: &Conversation, model_id: &str) -> Option<CompactionPlan> {
        self.plan_with(conversation, &self.budget(conversation, model_id))
    }

    /// Plan a background summary when the conversation is approaching (but has
    /// not reached) the trigger and the resolved strategy is agentic.
    pub fn plan_background(
        &self,
        conversation: &Conversation,
        model_id: &str,
    ) -> Option<CompactionPlan> {
        if !self.config.enabled || !self.config.background {
            return None;
        }
        if self.resolve_strategy(model_id, conversation.capabilities())
            != CompactionStrategyConfig::Agentic
        {
            return None;
        }
        let budget = self.budget(conversation, model_id);
        let tokens = conversation.estimated_tokens();
        if tokens >= budget.trigger_tokens || tokens < budget.trigger_tokens * 85 / 100 {
            return None;
        }
        self.plan_with(conversation, &budget)
    }

    fn plan_with(
        &self,
        conversation: &Conversation,
        budget: &CompactionBudget,
    ) -> Option<CompactionPlan> {
        let messages = conversation.messages();
        let cut = find_cut(
            messages,
            budget.preserve_recent_tokens,
            conversation.calibration_ratio(),
            &self.token_counter,
        )?;
        let head = messages[..cut].to_vec();

        let mut file_ops = self.file_ops.clone();
        file_ops.merge(&extract_file_ops(&head));
        let mut user_requests = self.user_requests.clone();
        for m in &head {
            if m.role == MessageRole::User && !is_summary_message(m) {
                user_requests.push(clip(m.content.trim(), 400));
            }
        }
        let excess = user_requests.len().saturating_sub(MAX_USER_REQUESTS);
        user_requests.drain(..excess);

        let max_output = conversation.capabilities().max_output.clamp(1024, 8192);
        Some(CompactionPlan {
            cut_index: cut,
            generation: conversation.generation(),
            previous_summary: head.first().and_then(previous_summary_body),
            head,
            file_ops,
            user_requests,
            max_chars: budget.max_tokens * 3,
            max_output_tokens: max_output,
        })
    }

    /// Replace the plan's head with a summary message, keeping the tail.
    ///
    /// Fails if the conversation was rewritten since the plan was made.
    pub fn apply_plan(
        &mut self,
        conversation: &mut Conversation,
        plan: CompactionPlan,
        body: String,
        strategy: CompactionStrategy,
        trigger: CompactionTrigger,
    ) -> Result<CompactionResult, CompactionError> {
        if conversation.generation() != plan.generation
            || plan.cut_index > conversation.message_count()
        {
            return Err(CompactionError::Failed(
                "conversation changed while compacting".into(),
            ));
        }
        let tokens_before = conversation.estimated_tokens();
        self.file_ops = plan.file_ops;
        self.user_requests = plan.user_requests;

        let live = self.live_state(conversation);
        let mut new_messages = vec![Message {
            role: MessageRole::User,
            content: format_summary_message(&body, &live),
            tool_calls: None,
            tool_call_id: None,
            ts: current_timestamp(),
            thinking: None,
        }];
        new_messages.extend(conversation.messages()[plan.cut_index..].iter().cloned());
        conversation.replace_messages(new_messages);

        let result = CompactionResult {
            strategy_used: strategy,
            tokens_before,
            tokens_after: conversation.estimated_tokens(),
            messages_removed: plan.cut_index,
            turns_summarized: plan
                .head
                .iter()
                .filter(|m| m.role == MessageRole::User && !is_summary_message(m))
                .count(),
            summary: body,
            native_provider: None,
            warning: None,
        };
        self.notify_observers(&result, trigger);
        Ok(result)
    }

    /// Sections regenerated from live state on every compaction.
    fn live_state(&self, conversation: &Conversation) -> String {
        let mut out = String::new();
        if let Some(todos) = &self.todos {
            out.push_str("## Todos\n");
            out.push_str(todos.trim());
            out.push_str("\n\n");
        }

        let n = self.config.recent_file_reinject;
        if n > 0 && conversation.cap().max_tokens >= MIN_WINDOW_FOR_REINJECT {
            let mut recent: Vec<&String> = Vec::new();
            for p in self
                .file_ops
                .modified_files
                .iter()
                .rev()
                .chain(self.file_ops.read_files.iter().rev())
            {
                if !recent.contains(&p) {
                    recent.push(p);
                }
            }
            let mut section = String::new();
            let mut count = 0;
            for path in recent {
                if count >= n {
                    break;
                }
                let full = match &self.project_root {
                    Some(root) => root.join(path),
                    None => PathBuf::from(path),
                };
                let Ok(content) = std::fs::read_to_string(&full) else {
                    continue;
                };
                section.push_str(&format!(
                    "### {path}\n```\n{}\n```\n",
                    truncate_output(&content, MAX_REINJECTED_FILE_CHARS).0
                ));
                count += 1;
            }
            if !section.is_empty() {
                out.push_str("## Recently touched files (current contents)\n");
                out.push_str(&section);
            }
        }
        out
    }

    /// Ask the provider to compact. Only checkpoints that can be replayed as
    /// plain messages are accepted; provider-specific blobs are ignored.
    async fn try_native_compaction<M: Model + ?Sized>(
        &self,
        model: &M,
        plan: &CompactionPlan,
        budget: &CompactionBudget,
        trigger: CompactionTrigger,
    ) -> Option<String> {
        let request = NativeCompactionRequest {
            messages: plan.head.iter().map(to_model_message).collect(),
            system_prompt: String::new(),
            tools: Vec::new(),
            keep_recent_tokens: budget.preserve_recent_tokens,
            trigger,
        };
        match model.compact_native(request).await {
            Ok(Some(NativeCompactionResponse {
                checkpoint: CompactionCheckpoint::ClientSummary { summary_message },
                ..
            })) if !summary_message.content.trim().is_empty() => Some(summary_message.content),
            _ => None,
        }
    }

    fn notify_observers(&self, result: &CompactionResult, trigger: CompactionTrigger) {
        if self.observers.is_empty() {
            return;
        }
        let event = CompactionEvent {
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
        for observer in &self.observers {
            observer.on_compaction(&event);
        }
    }
}

/// Choose the first message of the verbatim tail.
///
/// Returns `None` if there is nothing worth summarizing. The cut is moved so
/// that a tool result is never separated from the assistant message that
/// requested it.
fn find_cut(
    messages: &[Message],
    preserve_tokens: usize,
    ratio: f64,
    counter: &TokenCounter,
) -> Option<usize> {
    let n = messages.len();
    if n < 2 {
        return None;
    }

    let mut tail = 0usize;
    let mut cut = n;
    for (i, m) in messages.iter().enumerate().rev() {
        let t = (counter.count_event_message(m) as f64 * ratio).ceil() as usize;
        // The tail always keeps at least the newest message.
        if tail + t > preserve_tokens && cut < n {
            break;
        }
        tail += t;
        cut = i;
    }

    let mut c = cut;
    while c > 0 && messages[c].role == MessageRole::Tool {
        c -= 1;
    }
    if c == 0 {
        // The tail would start at the very beginning; summarize forward instead.
        c = cut;
        while c < n && messages[c].role == MessageRole::Tool {
            c += 1;
        }
        if c >= n {
            return None;
        }
    }
    if c == 0 || (c == 1 && is_summary_message(&messages[0])) {
        return None;
    }
    Some(c)
}

fn is_summary_message(m: &Message) -> bool {
    m.role == MessageRole::User && m.content.starts_with(SUMMARY_OPEN)
}

fn previous_summary_body(m: &Message) -> Option<String> {
    if !is_summary_message(m) {
        return None;
    }
    let inner = m
        .content
        .strip_prefix(SUMMARY_OPEN)?
        .trim_end()
        .trim_end_matches(SUMMARY_CLOSE);
    let body = inner.split(LIVE_STATE_DELIM).next().unwrap_or(inner).trim();
    (!body.is_empty()).then(|| body.to_string())
}

fn format_summary_message(body: &str, live: &str) -> String {
    let mut s = format!("{SUMMARY_OPEN}\n{}\n", body.trim());
    if !live.trim().is_empty() {
        s.push_str(LIVE_STATE_DELIM);
        s.push('\n');
        s.push_str(live.trim_end());
        s.push('\n');
    }
    s.push_str(SUMMARY_CLOSE);
    s
}

fn to_model_message(m: &Message) -> ModelMessage {
    ModelMessage {
        role: match m.role {
            MessageRole::User => ModelRole::User,
            MessageRole::Assistant => ModelRole::Assistant,
            MessageRole::Tool => ModelRole::Tool,
            MessageRole::System => ModelRole::System,
        },
        content: m.content.clone(),
        tool_calls: m.tool_calls.clone(),
        tool_call_id: m.tool_call_id.clone(),
        thinking: None,
    }
}

/// Extract file operations from a slice of messages (oldest first).
fn extract_file_ops(messages: &[Message]) -> FileOperationSummary {
    let mut ops = FileOperationSummary::default();
    for msg in messages {
        let Some(tool_calls) = &msg.tool_calls else {
            continue;
        };
        for tc in tool_calls {
            let Some(path) = tc.args.get("path").and_then(|v| v.as_str()) else {
                continue;
            };
            match tc.name.as_str() {
                "read" => touch(&mut ops.read_files, path),
                "write" | "edit" | "delete" => touch(&mut ops.modified_files, path),
                _ => {}
            }
        }
    }
    ops
}

/// Truncate to at most `max` bytes on a char boundary.
fn clip(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…[+{} bytes]", &s[..end], s.len() - end)
}

fn list_or_none(items: &[String]) -> String {
    if items.is_empty() {
        "none".to_string()
    } else {
        items.join(", ")
    }
}

/// Deterministic summary used by the basic strategy and as the fallback.
fn basic_body(plan: &CompactionPlan) -> String {
    let mut parts = vec![format!(
        "Earlier context ({} messages) was compacted. Work continues below.",
        plan.cut_index
    )];

    if !plan.user_requests.is_empty() {
        let list: Vec<String> = plan
            .user_requests
            .iter()
            .map(|r| format!("- {r}"))
            .collect();
        parts.push(format!(
            "## User requests (oldest first)\n{}",
            list.join("\n")
        ));
    }

    parts.push(format!(
        "## Files\nRead: {}\nEdited: {}",
        list_or_none(&plan.file_ops.read_files),
        list_or_none(&plan.file_ops.modified_files)
    ));

    let mut counts: HashMap<&str, usize> = HashMap::new();
    for tc in plan
        .head
        .iter()
        .filter_map(|m| m.tool_calls.as_ref())
        .flatten()
    {
        *counts.entry(tc.name.as_str()).or_insert(0) += 1;
    }
    if !counts.is_empty() {
        let mut items: Vec<_> = counts.into_iter().collect();
        items.sort();
        let list: Vec<String> = items.iter().map(|(n, c)| format!("{n} x{c}")).collect();
        parts.push(format!("## Tool activity\n{}", list.join(", ")));
    }

    parts.join("\n\n")
}

fn render_message(m: &Message) -> String {
    let (label, limit) = match m.role {
        MessageRole::User => ("User", MESSAGE_RENDER_CHARS),
        MessageRole::Assistant => ("Assistant", MESSAGE_RENDER_CHARS),
        MessageRole::Tool => ("Tool result", TOOL_RESULT_RENDER_CHARS),
        MessageRole::System => ("System", MESSAGE_RENDER_CHARS),
    };
    let mut out = format!("[{label}]: {}", clip(&m.content, limit));
    for tc in m.tool_calls.iter().flatten() {
        out.push_str(&format!(
            "\n  -> {}({})",
            tc.name,
            clip(&tc.args.to_string(), TOOL_ARGS_RENDER_CHARS)
        ));
    }
    out
}

/// Render messages for the summarizer, dropping the oldest to fit `max_chars`.
fn render_messages(messages: &[Message], max_chars: usize) -> String {
    let parts: Vec<String> = messages.iter().map(render_message).collect();
    let mut total: usize = parts.iter().map(|p| p.len() + 1).sum();
    let mut skip = 0;
    while total > max_chars && skip + 1 < parts.len() {
        total -= parts[skip].len() + 1;
        skip += 1;
    }
    let mut out = String::new();
    if skip > 0 {
        out.push_str(&format!("[{skip} earlier messages omitted]\n"));
    }
    out.push_str(&parts[skip..].join("\n"));
    out
}

fn build_summary_request(plan: &CompactionPlan) -> String {
    let mut parts = Vec::new();
    match &plan.previous_summary {
        Some(prev) => {
            parts.push(
                "Update the previous summary with the new conversation below. Keep what is still \
                 relevant, drop what is obsolete, and add new progress. Output the full updated \
                 summary."
                    .to_string(),
            );
            parts.push(format!("<previous_summary>\n{prev}\n</previous_summary>"));
        }
        None => parts.push(
            "Summarize this coding session so work can continue without the original messages."
                .to_string(),
        ),
    }
    parts.push(
        "Use exactly these sections, be concise and factual:\n\
         ## Goal\nWhat the user wants, in their terms.\n\
         ## Constraints & preferences\nInstructions or decisions the user gave that must keep applying.\n\
         ## Progress\n- Done / In progress / Blocked\n\
         ## Key findings\nTechnical facts, decisions and their reasons, errors seen (omit if none).\n\
         ## Next steps\nWhat to do next.\n\
         ## Files\nRead and edited files."
            .to_string(),
    );
    parts.push(format!(
        "Known file operations:\nRead: {}\nEdited: {}",
        list_or_none(&plan.file_ops.read_files),
        list_or_none(&plan.file_ops.modified_files)
    ));
    // The summarizer input keeps room for the instructions above.
    parts.push(format!(
        "<conversation>\n{}\n</conversation>",
        render_messages(&plan.head, plan.max_chars)
    ));
    parts.join("\n\n")
}

/// Generate the summary body for a plan with the model.
///
/// Free of any borrow on the conversation or manager, so it can run in a
/// spawned task.
pub async fn summarize_plan<M: Model + ?Sized>(
    model: &M,
    plan: &CompactionPlan,
) -> Result<String, CompactionError> {
    let messages = vec![
        ModelMessage {
            role: ModelRole::System,
            content: SUMMARIZER_SYSTEM_PROMPT.to_string(),
            tool_calls: None,
            tool_call_id: None,
            thinking: None,
        },
        ModelMessage {
            role: ModelRole::User,
            content: build_summary_request(plan),
            tool_calls: None,
            tool_call_id: None,
            thinking: None,
        },
    ];
    let params = crate::model::GenerationParams {
        temperature: Some(0.3),
        max_tokens: Some(plan.max_output_tokens),
        stop_sequences: None,
        top_p: Some(0.9),
    };
    let response = model.complete(&messages, &params, None).await?;
    let text = response.content.trim().to_string();
    if text.is_empty() {
        return Err(CompactionError::Failed(
            "summarizer returned an empty summary".into(),
        ));
    }
    if text.contains("## Files") {
        Ok(text)
    } else {
        Ok(format!(
            "{text}\n\n## Files\nRead: {}\nEdited: {}",
            list_or_none(&plan.file_ops.read_files),
            list_or_none(&plan.file_ops.modified_files)
        ))
    }
}

/// System prompt for the summarizer model.
const SUMMARIZER_SYSTEM_PROMPT: &str = r#"You are a context summarization assistant for a coding agent.
Your task is to create a concise, structured summary of a conversation that will be used
as context when the conversation continues. The summary must preserve all critical information
needed to continue the work seamlessly.

Follow the requested format exactly. Do NOT continue the conversation. Do NOT add commentary.
ONLY output the structured summary."#;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::Conversation;
    use crate::context::cap::ContextCap;
    use crate::events::{ToolCall, ToolCallStatus};
    use crate::model::MockModel;

    fn caps(tier: ModelTier, max: usize) -> ModelCapabilities {
        ModelCapabilities {
            tier,
            max_context: max,
            ..Default::default()
        }
    }

    fn conv(tier: ModelTier, max: usize) -> Conversation {
        Conversation::new(
            "System prompt".into(),
            ContextCap::new(max),
            caps(tier, max),
        )
    }

    fn call(id: &str, name: &str, path: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: name.into(),
            args: serde_json::json!({ "path": path }),
            status: ToolCallStatus::Pending,
        }
    }

    /// user, then n rounds of (assistant tool call, tool result, 400-char each)
    fn tool_conv(rounds: usize, max: usize) -> Conversation {
        let mut c = conv(ModelTier::Frontier, max);
        c.add_user_message("fix the bug in a.rs".into()).unwrap();
        for i in 0..rounds {
            c.add_assistant_message(
                "x".repeat(400),
                Some(vec![call(&format!("c{i}"), "read", &format!("f{i}.rs"))]),
                None,
            )
            .unwrap();
            c.add_tool_message("y".repeat(400), format!("c{i}"))
                .unwrap();
        }
        c
    }

    fn model() -> MockModel {
        MockModel::frontier("mock")
    }

    #[test]
    fn test_compaction_budget_ratio() {
        let c = conv(ModelTier::Frontier, 10000);
        let config = CompactionConfig {
            trigger_ratio: Some(0.75),
            target_ratio: 0.5,
            ..Default::default()
        };
        let b = CompactionBudget::from_conversation(&c, &config);
        assert_eq!(b.trigger_tokens, 7500);
        assert_eq!(b.target_tokens, 5000);
        assert_eq!(b.preserve_recent_tokens, 2500);
    }

    #[test]
    fn test_compaction_budget_reserve_scales_with_window() {
        let config = CompactionConfig::default();
        let big = CompactionBudget::from_conversation(&conv(ModelTier::Frontier, 200_000), &config);
        assert_eq!(big.trigger_tokens, 200_000 - 16_384);
        let small = CompactionBudget::from_conversation(&conv(ModelTier::Small, 4096), &config);
        assert_eq!(small.trigger_tokens, 3072);
        assert!(small.preserve_recent_tokens <= small.target_tokens / 2);
    }

    #[test]
    fn test_resolve_strategy_auto_and_overrides() {
        let mut config = CompactionConfig::default();
        config.model_overrides.insert(
            "special".into(),
            CompactionModelOverride {
                strategy: Some(CompactionStrategyConfig::Basic),
                preserve_recent_tokens: None,
            },
        );
        let manager = CompactionManager::new(config);
        let small = caps(ModelTier::Small, 4096);
        let frontier = caps(ModelTier::Frontier, 100_000);
        assert_eq!(
            manager.resolve_strategy("m", &small),
            CompactionStrategyConfig::Basic
        );
        assert_eq!(
            manager.resolve_strategy("m", &frontier),
            CompactionStrategyConfig::Agentic
        );
        assert_eq!(
            manager.resolve_strategy("special", &frontier),
            CompactionStrategyConfig::Basic
        );
    }

    #[test]
    fn test_file_ops_recency_order() {
        let msgs = vec![Message {
            role: MessageRole::Assistant,
            content: String::new(),
            tool_calls: Some(vec![
                call("1", "read", "a"),
                call("2", "write", "b"),
                call("3", "read", "c"),
                call("4", "read", "a"),
            ]),
            tool_call_id: None,
            ts: 0,
            thinking: None,
        }];
        let ops = extract_file_ops(&msgs);
        assert_eq!(ops.read_files, vec!["c", "a"]);
        assert_eq!(ops.modified_files, vec!["b"]);
    }

    #[test]
    fn test_cut_never_orphans_tool_results() {
        let c = tool_conv(6, 100_000);
        let counter = TokenCounter;
        for preserve in [1, 100, 250, 400, 700, 1000] {
            if let Some(cut) = find_cut(c.messages(), preserve, 1.0, &counter) {
                assert!(cut > 0);
                assert_ne!(
                    c.messages()[cut].role,
                    MessageRole::Tool,
                    "preserve={preserve}"
                );
            }
        }
    }

    #[test]
    fn test_cut_none_when_everything_fits() {
        let c = tool_conv(2, 100_000);
        assert!(find_cut(c.messages(), 1_000_000, 1.0, &TokenCounter).is_none());
    }

    #[test]
    fn test_cut_keeps_newest_message_even_if_huge() {
        let mut c = conv(ModelTier::Frontier, 100_000);
        c.add_user_message("first".into()).unwrap();
        c.add_assistant_message("ok".into(), None, None).unwrap();
        c.add_user_message("z".repeat(8000)).unwrap();
        assert_eq!(find_cut(c.messages(), 10, 1.0, &TokenCounter), Some(2));
    }

    #[tokio::test]
    async fn test_below_trigger_does_nothing_but_force_compacts() {
        let mut c = tool_conv(6, 100_000);
        let mut m = CompactionManager::new(CompactionConfig {
            preserve_recent_tokens: 300,
            ..Default::default()
        });
        let mock = model();
        mock.push_response(MockModel::text_response("## Goal\nfix\n## Files\nx"));

        assert!(
            m.maybe_compact(&mock, &mut c, CompactionTrigger::Auto)
                .await
                .unwrap()
                .is_none()
        );

        let r = m
            .compact_now(&mock, &mut c, CompactionTrigger::Manual)
            .await
            .unwrap()
            .expect("forced compaction runs");
        assert_eq!(r.strategy_used, CompactionStrategy::Agentic);
        assert!(r.messages_removed > 0);
        assert!(is_summary_message(&c.messages()[0]));
        assert_eq!(c.messages()[0].role, MessageRole::User);
        assert_ne!(c.messages()[1].role, MessageRole::Tool);
    }

    #[tokio::test]
    async fn test_agentic_failure_falls_back_to_basic() {
        let mut c = tool_conv(6, 100_000);
        let mut m = CompactionManager::new(CompactionConfig {
            preserve_recent_tokens: 300,
            ..Default::default()
        });
        let mock = model();
        mock.push_response(MockModel::error_response(ModelError::Provider(
            "boom".into(),
        )));
        let r = m
            .compact_now(&mock, &mut c, CompactionTrigger::Manual)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(r.strategy_used, CompactionStrategy::Basic);
        assert!(r.warning.is_some());
        assert!(c.messages()[0].content.contains("fix the bug in a.rs"));
    }

    #[tokio::test]
    async fn test_auto_trigger_prunes_before_summarizing() {
        let mut c = conv(ModelTier::Frontier, 20_000);
        c.add_user_message("go".into()).unwrap();
        for i in 0..10 {
            c.add_assistant_message(
                String::new(),
                Some(vec![call(&format!("c{i}"), "run", "x")]),
                None,
            )
            .unwrap();
            c.add_tool_message("o".repeat(7000), format!("c{i}"))
                .unwrap();
        }
        let mut m = CompactionManager::new(CompactionConfig {
            reserve_tokens: 4000,
            prune_protect_tokens: 2000,
            prune_min_savings_tokens: 500,
            ..Default::default()
        });
        let mock = model();
        assert!(c.estimated_tokens() >= m.trigger_tokens(&c));
        let r = m
            .maybe_compact(&mock, &mut c, CompactionTrigger::Auto)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(r.strategy_used, CompactionStrategy::Prune);
        assert!(r.tokens_after < r.tokens_before);
        assert_eq!(mock.remaining_responses(), 0);
    }

    #[test]
    fn test_iterative_summary_and_stale_plan() {
        let mut c = tool_conv(6, 100_000);
        let mut m = CompactionManager::new(CompactionConfig {
            preserve_recent_tokens: 300,
            ..Default::default()
        });
        let plan = m.plan(&c, "mock").unwrap();
        assert!(plan.previous_summary.is_none());
        let stale = plan.clone();
        let r = m
            .apply_plan(
                &mut c,
                plan,
                "## Goal\nfirst".into(),
                CompactionStrategy::Agentic,
                CompactionTrigger::Auto,
            )
            .unwrap();
        assert!(r.messages_removed > 0);

        // A plan made before the rewrite is rejected.
        assert!(
            m.apply_plan(
                &mut c,
                stale,
                "x".into(),
                CompactionStrategy::Agentic,
                CompactionTrigger::Auto
            )
            .is_err()
        );

        for i in 10..16 {
            c.add_assistant_message(
                "x".repeat(400),
                Some(vec![call(&format!("n{i}"), "read", "g.rs")]),
                None,
            )
            .unwrap();
            c.add_tool_message("y".repeat(400), format!("n{i}"))
                .unwrap();
        }
        let plan2 = m.plan(&c, "mock").unwrap();
        assert_eq!(plan2.previous_summary.as_deref(), Some("## Goal\nfirst"));
        assert!(plan2.file_ops.read_files.contains(&"f0.rs".to_string()));
        assert!(build_summary_request(&plan2).contains("<previous_summary>"));
    }

    #[test]
    fn test_summary_roundtrip_strips_live_state() {
        let msg = Message {
            role: MessageRole::User,
            content: format_summary_message("body text", "## Todos\n- a"),
            tool_calls: None,
            tool_call_id: None,
            ts: 0,
            thinking: None,
        };
        assert_eq!(previous_summary_body(&msg).as_deref(), Some("body text"));
    }

    #[test]
    fn test_summarizer_input_is_bounded() {
        let msgs: Vec<Message> = (0..50)
            .map(|i| Message {
                role: MessageRole::Tool,
                content: format!("{i}{}", "é".repeat(5000)),
                tool_calls: None,
                tool_call_id: Some("c".into()),
                ts: 0,
                thinking: None,
            })
            .collect();
        let out = render_messages(&msgs, 10_000);
        assert!(out.len() < 12_000);
        assert!(out.contains("earlier messages omitted"));
    }

    #[test]
    fn test_from_section_applies_overrides() {
        let section = crate::config::CompactionSection {
            trigger_ratio: Some(0.6),
            reserve_tokens: Some(1000),
            strategy: Some("basic".into()),
            background: Some(false),
            ..Default::default()
        };
        let c = CompactionConfig::from_section(Some(&section));
        assert_eq!(c.trigger_ratio, Some(0.6));
        assert_eq!(c.reserve_tokens, 1000);
        assert_eq!(c.strategy, CompactionStrategyConfig::Basic);
        assert!(!c.background);
        assert!(CompactionConfig::from_section(None).enabled);
    }
}
