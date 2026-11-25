//! Context compaction / summarization for long sessions.
//!
//! Provides two strategies:
//! - **Basic**: Deterministic truncation with dropped-work summaries (tool signatures, file ops).
//! - **Agentic**: LLM-based structured summarization with file operation tracking.
//! - **Native**: Delegation to provider compaction endpoints (OpenAI Responses, Anthropic).

use crate::context::tokens::TokenCounter;
use crate::context::{Conversation, ConversationError};
use crate::events::{CompactionEvent, CompactionStrategy, CompactionTrigger};
use crate::model::{Model, ModelCapabilities, ModelError, ModelMessage, ModelRole, ModelTier};
use crate::session::header::current_timestamp;
use crate::tool::tool_trait::ToolSpec;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use thiserror::Error;

/// Compaction configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactionConfig {
    /// Enable compaction.
    pub enabled: bool,
    /// Trigger ratio: compact when context reaches this fraction of the cap (default: 0.75).
    pub trigger_ratio: f32,
    /// Target ratio: compact down to this fraction of the cap (default: 0.5).
    pub target_ratio: f32,
    /// Number of recent tokens to preserve verbatim (default: 15000).
    pub preserve_recent_tokens: usize,
    /// Compaction strategy: "basic" | "agentic" | "auto" (tier-aware).
    pub strategy: CompactionStrategyConfig,
    /// Optional cheaper model for agentic summarization.
    pub summarizer_model: Option<String>,
    /// Per-provider native compaction settings.
    pub providers: CompactionProvidersConfig,
    /// Per-model strategy overrides.
    pub model_overrides: HashMap<String, CompactionModelOverride>,
}

impl Default for CompactionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            trigger_ratio: 0.75,
            target_ratio: 0.5,
            preserve_recent_tokens: 15000,
            strategy: CompactionStrategyConfig::Auto,
            summarizer_model: None,
            providers: CompactionProvidersConfig::default(),
            model_overrides: HashMap::new(),
        }
    }
}

/// Compaction strategy configuration.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum CompactionStrategyConfig {
    /// Deterministic: keep head + tail, insert dropped-work summaries.
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
}

impl CompactionBudget {
    /// Create a budget from conversation cap and config.
    pub fn from_conversation(conv: &Conversation, config: &CompactionConfig) -> Self {
        let max_tokens = conv.cap().max_tokens;
        let trigger_tokens = (max_tokens as f32 * config.trigger_ratio) as usize;
        let target_tokens = (max_tokens as f32 * config.target_ratio) as usize;
        Self {
            max_tokens,
            trigger_tokens,
            target_tokens,
            preserve_recent_tokens: config.preserve_recent_tokens,
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

/// Extract file operations from conversation messages.
#[derive(Debug, Clone, Default)]
pub struct FileOperationSummary {
    pub read_files: Vec<String>,
    pub modified_files: Vec<String>,
}

impl FileOperationSummary {
    pub fn merge(&mut self, other: &FileOperationSummary) {
        for f in &other.read_files {
            if !self.read_files.contains(f) {
                self.read_files.push(f.clone());
            }
        }
        for f in &other.modified_files {
            if !self.modified_files.contains(f) {
                self.modified_files.push(f.clone());
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.read_files.is_empty() && self.modified_files.is_empty()
    }
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

/// Main compaction manager.
pub struct CompactionManager {
    config: CompactionConfig,
    token_counter: TokenCounter,
    observers: Vec<Box<dyn CompactionObserver>>,
}

impl CompactionManager {
    /// Create a new compaction manager.
    pub fn new(config: CompactionConfig) -> Self {
        Self {
            config,
            token_counter: TokenCounter,
            observers: Vec::new(),
        }
    }

    /// Get the compaction configuration.
    pub fn config(&self) -> &CompactionConfig {
        &self.config
    }

    /// Register a compaction observer.
    pub fn register_observer(&mut self, observer: Box<dyn CompactionObserver>) {
        self.observers.push(observer);
    }

    /// Resolve the effective strategy for a given model.
    fn resolve_strategy(&self, capabilities: &ModelCapabilities) -> CompactionStrategyConfig {
        // Check model-specific override
        if let Some(override_) = self
            .config
            .model_overrides
            .get(&capabilities.tier.to_string())
            && let Some(s) = override_.strategy
        {
            return s;
        }
        // Check by model_id if available
        // Note: capabilities doesn't have model_id, but we can check tier
        match self.config.strategy {
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

    /// Check if compaction is needed and perform it.
    pub async fn maybe_compact<M: Model>(
        &mut self,
        model: &M,
        conversation: &mut Conversation,
        trigger: CompactionTrigger,
    ) -> Result<Option<CompactionResult>, CompactionError> {
        if !self.config.enabled {
            return Ok(None);
        }

        let budget = CompactionBudget::from_conversation(conversation, &self.config);
        if !budget.needs_compaction(conversation.estimated_tokens()) {
            return Ok(None);
        }

        let result = self.compact(model, conversation, trigger, &budget).await?;
        Ok(Some(result))
    }

    /// Perform compaction with the resolved strategy.
    async fn compact<M: Model>(
        &mut self,
        model: &M,
        conversation: &mut Conversation,
        trigger: CompactionTrigger,
        budget: &CompactionBudget,
    ) -> Result<CompactionResult, CompactionError> {
        let _tokens_before = conversation.estimated_tokens();
        let strategy_config = self.resolve_strategy(conversation.capabilities());

        // Try native compaction first if available and appropriate
        if matches!(
            trigger,
            CompactionTrigger::Auto | CompactionTrigger::OverflowRecovery
        ) && let Some(native_result) = self
            .try_native_compaction(model, conversation, budget)
            .await?
        {
            self.notify_observers(&native_result);
            return Ok(native_result);
        }

        // Fallback to client-side compaction
        let result = match strategy_config {
            CompactionStrategyConfig::Basic => {
                self.basic_compaction(conversation, budget, trigger).await?
            }
            CompactionStrategyConfig::Agentic => {
                self.agentic_compaction(model, conversation, budget, trigger)
                    .await?
            }
            CompactionStrategyConfig::Auto => unreachable!("Auto should be resolved"),
        };

        self.notify_observers(&result);
        Ok(result)
    }

    /// Try native provider compaction.
    async fn try_native_compaction<M: Model>(
        &self,
        _model: &M,
        _conversation: &Conversation,
        _budget: &CompactionBudget,
    ) -> Result<Option<CompactionResult>, CompactionError> {
        // Check if model supports native compaction
        // We can't directly check native compaction support from ModelCapabilities yet
        // This will be extended when providers implement the optional method
        // For now, return None to use client-side compaction
        Ok(None)
    }

    /// Basic deterministic compaction (Cline-inspired).
    /// Keeps head (system prompt, skills, todos) + tail (recent turns),
    /// inserts dropped-work summaries for the folded middle region.
    async fn basic_compaction(
        &self,
        conversation: &mut Conversation,
        budget: &CompactionBudget,
        _trigger: CompactionTrigger,
    ) -> Result<CompactionResult, CompactionError> {
        let original_messages = conversation.messages().to_vec();
        let original_count = original_messages.len();
        let tokens_before = conversation.estimated_tokens();

        // Calculate how many tokens we need to remove
        let target_tokens = budget.target_tokens.min(budget.trigger_tokens);
        let system_tokens = self
            .token_counter
            .count_message("system", conversation.system_prompt());
        let file_tokens: usize = conversation.included_files().iter().map(|f| f.tokens).sum();
        let available_for_history = target_tokens.saturating_sub(system_tokens + file_tokens);

        // We need to keep: system prompt + skills + recent tokens (tail)
        // Fold the middle region
        let preserve_recent = budget.preserve_recent_tokens.min(available_for_history);

        // Walk backwards from the end to find cut point
        let mut tail_tokens = 0;
        let mut cut_index = original_count;
        for (i, msg) in original_messages.iter().enumerate().rev() {
            let msg_tokens = self
                .token_counter
                .count_message(&msg.role.to_string(), &msg.content);
            if tail_tokens + msg_tokens > preserve_recent {
                break;
            }
            tail_tokens += msg_tokens;
            cut_index = i;
        }

        // If we can't cut (everything fits in preserve_recent), just return
        if cut_index == 0 || cut_index >= original_count {
            return Ok(CompactionResult {
                strategy_used: CompactionStrategy::Basic,
                tokens_before,
                tokens_after: tokens_before,
                messages_removed: 0,
                turns_summarized: 0,
                summary: "No compaction needed".to_string(),
                native_provider: None,
            });
        }

        // Extract file operations from the folded region
        let file_ops = self.extract_file_ops(&original_messages[..cut_index]);

        // Build dropped-work summary message
        let dropped_summary =
            self.build_dropped_work_summary(&original_messages[..cut_index], &file_ops, cut_index);

        // Build new message list: dropped summary + tail
        let mut new_messages = Vec::new();

        // Add the dropped-work summary as a system message at the start of history
        new_messages.push(crate::events::Message {
            role: crate::events::MessageRole::System,
            content: dropped_summary,
            tool_calls: None,
            tool_call_id: None,
            ts: current_timestamp(),
            thinking: None,
        });

        // Add the preserved tail messages
        for msg in &original_messages[cut_index..] {
            new_messages.push(msg.clone());
        }

        // Apply the new messages to the conversation
        conversation.replace_messages(new_messages);
        let tokens_after = conversation.estimated_tokens();

        Ok(CompactionResult {
            strategy_used: CompactionStrategy::Basic,
            tokens_before,
            tokens_after,
            messages_removed: cut_index,
            turns_summarized: cut_index / 2, // Approximate user-assistant pairs
            summary: format!(
                "Basic compaction: folded {} messages, preserved {} recent tokens",
                cut_index, tail_tokens
            ),
            native_provider: None,
        })
    }

    /// Agentic LLM-based compaction (OpenCode/Codex-inspired).
    /// Uses a (potentially cheaper) model to generate a structured summary.
    async fn agentic_compaction<M: Model>(
        &self,
        model: &M,
        conversation: &mut Conversation,
        budget: &CompactionBudget,
        _trigger: CompactionTrigger,
    ) -> Result<CompactionResult, CompactionError> {
        // Check if we have a summarizer model configured
        let _summarizer_model_id = self
            .config
            .summarizer_model
            .as_deref()
            .ok_or(CompactionError::SummarizerNotConfigured)?;

        // For now, we'll use the same model (in practice, you'd create a separate Model instance)
        // This requires the summarizer to be accessible. We'll simulate by using the current model.

        let original_messages = conversation.messages().to_vec();
        let original_count = original_messages.len();
        let tokens_before = conversation.estimated_tokens();

        // Find cut index at preserve_recent_tokens boundary
        let mut tail_tokens = 0;
        let mut cut_index = original_count;
        for (i, msg) in original_messages.iter().enumerate().rev() {
            let msg_tokens = self
                .token_counter
                .count_message(&msg.role.to_string(), &msg.content);
            if tail_tokens + msg_tokens > budget.preserve_recent_tokens {
                break;
            }
            tail_tokens += msg_tokens;
            cut_index = i;
        }

        if cut_index == 0 {
            return Ok(CompactionResult {
                strategy_used: CompactionStrategy::Agentic,
                tokens_before,
                tokens_after: tokens_before,
                messages_removed: 0,
                turns_summarized: 0,
                summary: "No compaction needed".to_string(),
                native_provider: None,
            });
        }

        // Extract messages to summarize
        let messages_to_summarize = &original_messages[..cut_index];

        // Extract file operations
        let file_ops = self.extract_file_ops(messages_to_summarize);

        // Build summary request
        let summary_request = self.build_summary_request(
            messages_to_summarize,
            &file_ops,
            conversation.system_prompt(),
        );

        // Call the model to generate summary
        // Note: In practice, you'd want a separate summarizer model instance.
        // For now, we use the same model with a special prompt.
        let summary_messages = vec![
            ModelMessage {
                role: ModelRole::System,
                content: SUMMARIZER_SYSTEM_PROMPT.to_string(),
                tool_calls: None,
                tool_call_id: None,
                thinking: None,
            },
            ModelMessage {
                role: ModelRole::User,
                content: summary_request,
                tool_calls: None,
                tool_call_id: None,
                thinking: None,
            },
        ];

        let generation_params = crate::model::GenerationParams {
            temperature: Some(0.3),
            max_tokens: Some(8192),
            stop_sequences: None,
            top_p: Some(0.9),
        };

        let response = model
            .complete(&summary_messages, &generation_params, None)
            .await
            .map_err(CompactionError::Model)?;

        let summary_text = response.content.trim().to_string();
        if summary_text.is_empty() {
            return Err(CompactionError::Failed(
                "Summarizer returned empty summary".into(),
            ));
        }

        // Ensure Files section is present
        let summary_text = self.ensure_files_section(&summary_text, &file_ops);

        // Build summary message
        let summary_message = crate::events::Message {
            role: crate::events::MessageRole::System,
            content: format!("Context summary:\n\n{}", summary_text),
            tool_calls: None,
            tool_call_id: None,
            ts: current_timestamp(),
            thinking: None,
        };

        // Build new message list: summary message + tail
        let mut new_messages = Vec::new();
        new_messages.push(summary_message);
        for msg in &original_messages[cut_index..] {
            new_messages.push(msg.clone());
        }

        // Apply the new messages to the conversation
        conversation.replace_messages(new_messages);
        let tokens_after = conversation.estimated_tokens();

        Ok(CompactionResult {
            strategy_used: CompactionStrategy::Agentic,
            tokens_before,
            tokens_after,
            messages_removed: cut_index,
            turns_summarized: cut_index / 2,
            summary: summary_text,
            native_provider: None,
        })
    }

    /// Extract file operations from a slice of messages.
    fn extract_file_ops(&self, messages: &[crate::events::Message]) -> FileOperationSummary {
        let mut ops = FileOperationSummary::default();
        for msg in messages {
            // Check tool calls for file operations
            if let Some(tool_calls) = &msg.tool_calls {
                for tc in tool_calls {
                    match tc.name.as_str() {
                        "read" => {
                            if let Some(path) = tc.args.get("path").and_then(|v| v.as_str())
                                && !ops.read_files.contains(&path.to_string())
                            {
                                ops.read_files.push(path.to_string());
                            }
                        }
                        "write" | "edit" | "delete" => {
                            if let Some(path) = tc.args.get("path").and_then(|v| v.as_str())
                                && !ops.modified_files.contains(&path.to_string())
                            {
                                ops.modified_files.push(path.to_string());
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        ops
    }

    /// Build a dropped-work summary for basic compaction.
    fn build_dropped_work_summary(
        &self,
        folded_messages: &[crate::events::Message],
        file_ops: &FileOperationSummary,
        _cut_index: usize,
    ) -> String {
        let mut parts = Vec::new();

        parts.push(format!(
            "<SYSTEM_NOTICE>\nEarlier context was compacted. Summary of {} messages (approx. {} turns) before the current conversation:\n",
            folded_messages.len(),
            folded_messages.len() / 2
        ));

        if !file_ops.read_files.is_empty() {
            parts.push(format!("Files read: {}", file_ops.read_files.join(", ")));
        }
        if !file_ops.modified_files.is_empty() {
            parts.push(format!(
                "Files modified: {}",
                file_ops.modified_files.join(", ")
            ));
        }

        // Count tool calls
        let mut tool_counts = HashMap::new();
        for msg in folded_messages {
            if let Some(tool_calls) = &msg.tool_calls {
                for tc in tool_calls {
                    *tool_counts.entry(tc.name.clone()).or_insert(0) += 1;
                }
            }
        }
        if !tool_counts.is_empty() {
            let tool_summary: Vec<String> = tool_counts
                .into_iter()
                .map(|(name, count)| format!("{name} x{count}"))
                .collect();
            parts.push(format!("Tool calls: {}", tool_summary.join(", ")));
        }

        parts.push(
            "Conversation from this point onward continues verbatim below.</SYSTEM_NOTICE>"
                .to_string(),
        );

        parts.join("\n")
    }

    /// Build summary request for agentic compaction.
    fn build_summary_request(
        &self,
        messages: &[crate::events::Message],
        file_ops: &FileOperationSummary,
        _system_prompt: &str,
    ) -> String {
        let mut parts = vec![
            "Summarize this coding session for continuation. Be concise and factual.".to_string(),
            "".to_string(),
            "## Goal".to_string(),
            "One sentence: what is being built or fixed.".to_string(),
            "".to_string(),
            "## State".to_string(),
            "- Done: completed steps".to_string(),
            "- In Progress: current work".to_string(),
            "- Blocked: blockers or open questions".to_string(),
            "".to_string(),
            "## Highlights".to_string(),
            "Key technical choices or notable findings (omit if none).".to_string(),
            "".to_string(),
            "## Next".to_string(),
            "Immediate next steps.".to_string(),
            "".to_string(),
            "## Files".to_string(),
            format!(
                "Read: {}",
                if file_ops.read_files.is_empty() {
                    "none".to_string()
                } else {
                    file_ops.read_files.join(", ")
                }
            ),
            format!(
                "Edited: {}",
                if file_ops.modified_files.is_empty() {
                    "none".to_string()
                } else {
                    file_ops.modified_files.join(", ")
                }
            ),
            "".to_string(),
            "Conversation:".to_string(),
        ];

        // Serialize conversation
        for msg in messages {
            let role = match msg.role {
                crate::events::MessageRole::User => "User",
                crate::events::MessageRole::Assistant => "Assistant",
                crate::events::MessageRole::Tool => "Tool",
                crate::events::MessageRole::System => "System",
            };
            parts.push(format!("[{}]: {}", role, msg.content));
        }

        parts.join("\n")
    }

    /// Ensure Files section is present in summary.
    fn ensure_files_section(&self, summary: &str, file_ops: &FileOperationSummary) -> String {
        if summary.contains("## Files") {
            return summary.to_string();
        }

        let read_str = if file_ops.read_files.is_empty() {
            "none".to_string()
        } else {
            file_ops.read_files.join(", ")
        };
        let mod_str = if file_ops.modified_files.is_empty() {
            "none".to_string()
        } else {
            file_ops.modified_files.join(", ")
        };

        format!(
            "{}\n\n## Files\nRead: {}\nEdited: {}",
            summary.trim(),
            read_str,
            mod_str
        )
    }

    /// Notify all observers of compaction event.
    fn notify_observers(&self, result: &CompactionResult) {
        let event = CompactionEvent {
            trigger: CompactionTrigger::Auto, // Will be overridden by caller
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
    use crate::model::{ModelCapabilities, ModelTier};

    #[test]
    fn test_compaction_budget() {
        let cap = crate::context::cap::ContextCap::new(10000);
        let capabilities = ModelCapabilities {
            tier: ModelTier::Frontier,
            max_context: 10000,
            ..Default::default()
        };
        let conv = Conversation::new("System prompt".into(), cap, capabilities);

        let config = CompactionConfig {
            trigger_ratio: 0.75,
            target_ratio: 0.5,
            preserve_recent_tokens: 15000,
            ..Default::default()
        };

        let budget = CompactionBudget::from_conversation(&conv, &config);
        assert_eq!(budget.max_tokens, 10000);
        assert_eq!(budget.trigger_tokens, 7500);
        assert_eq!(budget.target_tokens, 5000);
    }

    #[test]
    fn test_resolve_strategy_auto() {
        let config = CompactionConfig::default();
        let manager = CompactionManager::new(config);

        let small_caps = ModelCapabilities {
            tier: ModelTier::Small,
            ..Default::default()
        };
        let frontier_caps = ModelCapabilities {
            tier: ModelTier::Frontier,
            ..Default::default()
        };

        assert_eq!(
            manager.resolve_strategy(&small_caps),
            CompactionStrategyConfig::Basic
        );
        assert_eq!(
            manager.resolve_strategy(&frontier_caps),
            CompactionStrategyConfig::Agentic
        );
    }

    #[test]
    fn test_file_ops_extraction() {
        let config = CompactionConfig::default();
        let manager = CompactionManager::new(config);

        let messages = vec![crate::events::Message {
            role: crate::events::MessageRole::Assistant,
            content: "".into(),
            tool_calls: Some(vec![
                crate::events::ToolCall {
                    id: "1".into(),
                    name: "read".into(),
                    args: serde_json::json!({"path": "/tmp/a.txt"}),
                    status: crate::events::ToolCallStatus::Pending,
                },
                crate::events::ToolCall {
                    id: "2".into(),
                    name: "write".into(),
                    args: serde_json::json!({"path": "/tmp/b.txt", "content": "hello"}),
                    status: crate::events::ToolCallStatus::Pending,
                },
            ]),
            tool_call_id: None,
            ts: 0,
            thinking: None,
        }];

        let ops = manager.extract_file_ops(&messages);
        assert_eq!(ops.read_files, vec!["/tmp/a.txt"]);
        assert_eq!(ops.modified_files, vec!["/tmp/b.txt"]);
    }
}
