//! Conversation context management.
//!
//! Handles the system prompt, skills directory concatenation,
//! message history, token accounting (calibrated against provider usage),
//! cost estimation, hard context cap enforcement, tiered prompts,
//! tool-result pruning, and thinking channel handling.
//!
//! The history sent to the model is append-only between compactions so the
//! request prefix stays byte-stable (provider prompt caches rely on this).

use crate::context::cap::{ContextCap, ContextCapError};
use crate::context::selective::SelectiveContextConfig;
use crate::context::tier::TierConfig;
use crate::context::tokens::{CostEstimate, TokenCounter};
use crate::context::warnings::WarningLevel;
use crate::events::{Message, MessageRole};
use crate::model::{ModelCapabilities, ModelTier};
use std::path::{Path, PathBuf};
use thiserror::Error;

/// Prefix of the placeholder that replaces a pruned tool result.
pub const PRUNED_TOOL_OUTPUT_PREFIX: &str = "[tool output cleared";

/// Tool results smaller than this (in estimated tokens) are not worth pruning.
const MIN_PRUNABLE_TOOL_TOKENS: usize = 60;

/// Bounds for the estimate-to-reality correction factor.
const MIN_CALIBRATION_RATIO: f64 = 0.25;
const MAX_CALIBRATION_RATIO: f64 = 4.0;

/// Errors related to conversation context.
#[derive(Debug, Error)]
pub enum ConversationError {
    #[error("context cap exceeded: {0}")]
    ContextCap(#[from] ContextCapError),
    #[error("failed to read skills directory {path}: {source}")]
    SkillsDirRead {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("skills directory not found: {path}")]
    SkillsDirNotFound { path: PathBuf },
    #[error("failed to read skill file {path}: {source}")]
    SkillFileRead {
        path: PathBuf,
        source: std::io::Error,
    },
}

/// System prompt source.
#[derive(Debug, Clone)]
pub enum SystemPromptSource {
    /// Static built-in prompt template.
    Builtin(String),
    /// Custom prompt from file.
    File(PathBuf),
}

/// Outcome of pruning old tool results.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PruneStats {
    /// Number of tool results replaced by a placeholder.
    pub messages_pruned: usize,
    /// Estimated tokens freed.
    pub tokens_saved: usize,
}

/// A conversation with context management.
#[derive(Debug, Clone)]
pub struct Conversation {
    /// The system prompt (static + skills).
    system_prompt: String,
    /// Message history (excluding system prompt).
    messages: Vec<Message>,
    /// Context cap enforcement.
    cap: ContextCap,
    /// Token counter.
    token_counter: TokenCounter,
    /// Model capabilities for tier-aware behavior.
    capabilities: ModelCapabilities,
    /// Tier configuration (prompt template, tool subset, generation params).
    tier_config: TierConfig,
    /// Selective context configuration.
    selective_config: SelectiveContextConfig,
    /// Estimated tokens of the tool schemas sent with every request.
    tool_overhead_tokens: usize,
    /// Correction factor (provider-reported / heuristic) applied to estimates.
    calibration_ratio: f64,
    /// Provider-reported token count and the message count it covered.
    anchor: Option<(usize, usize)>,
    /// Bumped whenever the history is rewritten (compaction, clear).
    generation: u64,
    /// Total estimated tokens in context (system + tools + messages).
    estimated_tokens: usize,
    /// Current cost estimate.
    cost_estimate: CostEstimate,
}

impl Conversation {
    /// Create a new conversation with the given system prompt and context cap.
    pub fn new(system_prompt: String, cap: ContextCap, capabilities: ModelCapabilities) -> Self {
        let tier_config = TierConfig::from_capabilities(&capabilities);
        let selective_config = tier_config.selective_context.clone();
        let token_counter = TokenCounter;
        let system_tokens = token_counter.count_message("system", &system_prompt);

        Self {
            system_prompt,
            messages: Vec::new(),
            cap,
            token_counter,
            capabilities,
            tier_config,
            selective_config,
            tool_overhead_tokens: 0,
            calibration_ratio: 1.0,
            anchor: None,
            generation: 0,
            estimated_tokens: system_tokens,
            cost_estimate: CostEstimate::default(),
        }
    }

    /// Create a conversation from a model tier (uses built-in prompt template).
    pub fn from_tier(tier: ModelTier, max_context: usize) -> Self {
        let system_prompt = system_prompt_for_tier(tier).to_string();
        let capabilities = ModelCapabilities {
            tier,
            max_context,
            ..Default::default()
        };
        let cap = ContextCap::new(max_context);
        Self::new(system_prompt, cap, capabilities)
    }

    /// Create a conversation with a custom system prompt.
    pub fn with_system_prompt(
        system_prompt: String,
        max_context: usize,
        capabilities: ModelCapabilities,
    ) -> Self {
        let cap = ContextCap::new(max_context);
        Self::new(system_prompt, cap, capabilities)
    }

    /// Get the system prompt.
    pub fn system_prompt(&self) -> &str {
        &self.system_prompt
    }

    /// Get the message history.
    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    /// Get the context cap.
    pub fn cap(&self) -> &ContextCap {
        &self.cap
    }

    /// Get the model capabilities.
    pub fn capabilities(&self) -> &ModelCapabilities {
        &self.capabilities
    }

    /// Get the tier configuration.
    pub fn tier_config(&self) -> &TierConfig {
        &self.tier_config
    }

    /// Get the selective context configuration.
    pub fn selective_config(&self) -> &SelectiveContextConfig {
        &self.selective_config
    }

    /// Get the current estimated token count.
    pub fn estimated_tokens(&self) -> usize {
        self.estimated_tokens
    }

    /// Get the current cost estimate.
    pub fn cost_estimate(&self) -> &CostEstimate {
        &self.cost_estimate
    }

    /// Get the context warning level.
    pub fn warning_level(&self) -> WarningLevel {
        WarningLevel::from_tokens(&self.cap, self.estimated_tokens)
    }

    /// Get remaining tokens before cap.
    pub fn remaining_tokens(&self) -> usize {
        self.cap.remaining(self.estimated_tokens)
    }

    /// Counter that increments whenever the history is rewritten.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Current estimate-correction factor (1.0 until the first provider usage report).
    pub fn calibration_ratio(&self) -> f64 {
        self.calibration_ratio
    }

    /// Tell the conversation how many tokens the tool schemas add to each request.
    pub fn set_tool_overhead(&mut self, tokens: usize) {
        self.tool_overhead_tokens = tokens;
        self.recalculate_tokens();
    }

    fn scaled(&self, tokens: usize) -> usize {
        (tokens as f64 * self.calibration_ratio).ceil() as usize
    }

    fn message_tokens(&self, msg: &Message) -> usize {
        self.scaled(self.token_counter.count_event_message(msg))
    }

    fn fixed_tokens_raw(&self) -> usize {
        self.token_counter
            .count_message("system", &self.system_prompt)
            + self.tool_overhead_tokens
    }

    /// Add a user message.
    pub fn add_user_message(&mut self, content: String) -> Result<(), ConversationError> {
        let msg = Message {
            role: MessageRole::User,
            content,
            tool_calls: None,
            tool_call_id: None,
            ts: current_timestamp(),
            thinking: None,
        };
        self.add_message(msg)
    }

    /// Add an assistant message.
    pub fn add_assistant_message(
        &mut self,
        content: String,
        tool_calls: Option<Vec<crate::events::ToolCall>>,
        thinking: Option<String>,
    ) -> Result<(), ConversationError> {
        let msg = Message {
            role: MessageRole::Assistant,
            content,
            tool_calls,
            tool_call_id: None,
            ts: current_timestamp(),
            thinking,
        };
        self.add_message(msg)
    }

    /// Add a tool result message.
    pub fn add_tool_message(
        &mut self,
        content: String,
        tool_call_id: String,
    ) -> Result<(), ConversationError> {
        let msg = Message {
            role: MessageRole::Tool,
            content,
            tool_calls: None,
            tool_call_id: Some(tool_call_id),
            ts: current_timestamp(),
            thinking: None,
        };
        self.add_message(msg)
    }

    /// Add a message with cap enforcement.
    fn add_message(&mut self, msg: Message) -> Result<(), ConversationError> {
        let msg_tokens = self.message_tokens(&msg);

        // Check if adding this message would exceed the cap
        self.cap.enforce(self.estimated_tokens, msg_tokens)?;

        self.estimated_tokens += msg_tokens;
        self.messages.push(msg);
        Ok(())
    }

    /// Add a message with cost tracking.
    pub fn add_message_with_cost(
        &mut self,
        msg: Message,
        cost: CostEstimate,
    ) -> Result<(), ConversationError> {
        let msg_tokens = self.message_tokens(&msg);
        self.cap
            .enforce_with_cost(self.estimated_tokens, msg_tokens, &cost)?;
        self.estimated_tokens += msg_tokens;
        self.cost_estimate.input_tokens += cost.input_tokens;
        self.cost_estimate.output_tokens += cost.output_tokens;
        self.cost_estimate.input_cost_usd += cost.input_cost_usd;
        self.cost_estimate.output_cost_usd += cost.output_cost_usd;
        self.messages.push(msg);
        Ok(())
    }

    /// Clear history but keep system prompt.
    pub fn clear_history(&mut self) {
        self.messages.clear();
        self.anchor = None;
        self.generation += 1;
        self.recalculate_tokens();
        self.cost_estimate = CostEstimate::default();
    }

    /// Get the number of messages in history (excluding system).
    pub fn message_count(&self) -> usize {
        self.messages.len()
    }

    /// Recalculate total estimated tokens (system + tools + messages).
    ///
    /// When a provider-reported count covers a prefix of the history, only the
    /// messages added since are estimated; otherwise everything is estimated
    /// and scaled by the calibration ratio.
    pub fn recalculate_tokens(&mut self) {
        self.estimated_tokens = match self.anchor {
            Some((tokens, count)) if count <= self.messages.len() => {
                tokens
                    + self.messages[count..]
                        .iter()
                        .map(|m| self.message_tokens(m))
                        .sum::<usize>()
            }
            _ => {
                let raw: usize = self.fixed_tokens_raw()
                    + self
                        .messages
                        .iter()
                        .map(|m| self.token_counter.count_event_message(m))
                        .sum::<usize>();
                self.scaled(raw)
            }
        };
    }

    /// Record provider-reported usage for the request that produced the last
    /// assistant message. Call right after that message was added.
    ///
    /// `prompt_tokens` must include cached tokens. Anchors the estimate on
    /// the real count and updates the correction factor used for later
    /// estimates (including after the history is rewritten).
    pub fn record_usage(&mut self, prompt_tokens: usize, completion_tokens: usize) {
        if prompt_tokens == 0 {
            return;
        }
        let actual = prompt_tokens + completion_tokens;
        let raw: usize = self.fixed_tokens_raw()
            + self
                .messages
                .iter()
                .map(|m| self.token_counter.count_event_message(m))
                .sum::<usize>();
        if raw > 0 {
            self.calibration_ratio =
                (actual as f64 / raw as f64).clamp(MIN_CALIBRATION_RATIO, MAX_CALIBRATION_RATIO);
        }
        self.anchor = Some((actual, self.messages.len()));
        self.estimated_tokens = actual;
    }

    /// Replace message history (used for compaction).
    pub fn replace_messages(&mut self, messages: Vec<Message>) {
        self.messages = messages;
        self.anchor = None;
        self.generation += 1;
        self.recalculate_tokens();
    }

    /// Replace the content of old tool results with a short placeholder.
    ///
    /// The most recent `protect_tokens` worth of tool output is left intact.
    /// Nothing is changed unless at least `min_savings` tokens would be freed.
    /// Message order and tool-call pairing are preserved.
    pub fn prune_tool_results(
        &mut self,
        protect_tokens: usize,
        min_savings: usize,
    ) -> Option<PruneStats> {
        let mut protected = 0usize;
        let mut candidates: Vec<(usize, usize)> = Vec::new();
        for (i, msg) in self.messages.iter().enumerate().rev() {
            if msg.role != MessageRole::Tool || msg.content.starts_with(PRUNED_TOOL_OUTPUT_PREFIX) {
                continue;
            }
            let tokens = self.token_counter.count_event_message(msg);
            if protected < protect_tokens {
                protected += tokens;
                continue;
            }
            if tokens >= MIN_PRUNABLE_TOOL_TOKENS {
                candidates.push((i, tokens));
            }
        }

        let placeholder_tokens = |bytes: usize| {
            self.token_counter
                .count_message("tool", &pruned_placeholder(bytes))
        };
        let saved: usize = candidates
            .iter()
            .map(|&(i, t)| t.saturating_sub(placeholder_tokens(self.messages[i].content.len())))
            .sum();
        let saved = self.scaled(saved);
        if candidates.is_empty() || saved < min_savings.max(1) {
            return None;
        }

        for &(i, _) in &candidates {
            let bytes = self.messages[i].content.len();
            self.messages[i].content = pruned_placeholder(bytes);
        }
        self.anchor = None;
        self.recalculate_tokens();
        Some(PruneStats {
            messages_pruned: candidates.len(),
            tokens_saved: saved,
        })
    }

    /// Token usage breakdown for status display: (system, history, tool schemas).
    pub fn context_breakdown(&self) -> (usize, usize, usize) {
        let system_tokens = self
            .token_counter
            .count_message("system", &self.system_prompt);
        let history_tokens: usize = self
            .messages
            .iter()
            .map(|m| self.token_counter.count_event_message(m))
            .sum();
        (system_tokens, history_tokens, self.tool_overhead_tokens)
    }

    /// Get messages for the model, applying thinking channel rules.
    ///
    /// The whole history is sent; compaction is the only thing that drops
    /// messages, which keeps the request prefix stable for prompt caching.
    pub fn to_model_messages(&self) -> Vec<crate::model::ModelMessage> {
        let mut result = Vec::with_capacity(self.messages.len() + 1);

        result.push(crate::model::ModelMessage {
            role: crate::model::ModelRole::System,
            content: self.system_prompt.clone(),
            tool_calls: None,
            tool_call_id: None,
            thinking: None,
        });

        for msg in &self.messages {
            result.push(crate::model::ModelMessage {
                role: match msg.role {
                    MessageRole::User => crate::model::ModelRole::User,
                    MessageRole::Assistant => crate::model::ModelRole::Assistant,
                    MessageRole::Tool => crate::model::ModelRole::Tool,
                    MessageRole::System => crate::model::ModelRole::System,
                },
                content: msg.content.clone(),
                tool_calls: msg.tool_calls.clone(),
                tool_call_id: msg.tool_call_id.clone(),
                // Replay thinking only for frontier models (tier-gated)
                thinking: if self.tier_config.replay_thinking {
                    msg.thinking.clone()
                } else {
                    None
                },
            });
        }

        result
    }

    /// Check if a tool is allowed for this conversation's tier.
    pub fn allows_tool(&self, tool_name: &str) -> bool {
        self.tier_config.allows_tool(tool_name)
    }

    /// Get allowed tool names for this tier.
    pub fn allowed_tools(&self) -> Vec<String> {
        self.tier_config.allowed_tool_names()
    }

    /// Get generation parameters for this tier.
    pub fn generation_params(&self) -> crate::model::GenerationParams {
        self.tier_config.generation_params.clone()
    }
}

fn pruned_placeholder(bytes: usize) -> String {
    format!(
        "{PRUNED_TOOL_OUTPUT_PREFIX} to save context ({bytes} bytes); re-run the tool if you still need it]"
    )
}

/// Load and concatenate skills from a directory.
pub fn load_skills(skills_dir: &Path) -> Result<String, ConversationError> {
    if !skills_dir.exists() {
        return Err(ConversationError::SkillsDirNotFound {
            path: skills_dir.to_path_buf(),
        });
    }

    if !skills_dir.is_dir() {
        return Err(ConversationError::SkillsDirNotFound {
            path: skills_dir.to_path_buf(),
        });
    }

    let mut entries: Vec<_> = std::fs::read_dir(skills_dir)
        .map_err(|e| ConversationError::SkillsDirRead {
            path: skills_dir.to_path_buf(),
            source: e,
        })?
        .filter_map(|e| e.ok())
        .collect();

    // Sort by filename for deterministic order
    entries.sort_by_key(|e| e.file_name());

    let mut skills_content = String::new();

    for entry in entries {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) == Some("md") {
            let content =
                std::fs::read_to_string(&path).map_err(|e| ConversationError::SkillFileRead {
                    path: path.clone(),
                    source: e,
                })?;
            skills_content.push_str(&format!(
                "\n\n---\n\n# {}\n\n{}",
                entry.file_name().to_string_lossy(),
                content
            ));
        }
    }

    Ok(skills_content)
}

/// Build the system prompt for a tier, including skills if configured.
pub fn build_system_prompt(
    tier: ModelTier,
    skills_dir: Option<&Path>,
) -> Result<String, ConversationError> {
    let mut prompt = system_prompt_for_tier(tier).to_string();

    if let Some(dir) = skills_dir {
        let skills = load_skills(dir)?;
        if !skills.is_empty() {
            prompt.push_str("\n\n## Skills\n");
            prompt.push_str(&skills);
        }
    }

    Ok(prompt)
}

/// Like [`build_system_prompt`], plus durable project instructions and the
/// memory index. The result is fixed for the session (byte-stable prefix).
pub fn build_system_prompt_with_context(
    tier: ModelTier,
    skills_dir: Option<&Path>,
    project: &crate::context::memory::ProjectContext,
) -> Result<String, ConversationError> {
    let mut prompt = build_system_prompt(tier, skills_dir)?;
    prompt.push_str(&project.render());
    Ok(prompt)
}
fn system_prompt_for_tier(tier: ModelTier) -> &'static str {
    match tier {
        ModelTier::Small => include_str!("../../assets/prompts/small_system_prompt.txt"),
        ModelTier::Mid => include_str!("../../assets/prompts/mid_system_prompt.txt"),
        ModelTier::Frontier => include_str!("../../assets/prompts/frontier_system_prompt.txt"),
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
    use crate::events::MessageRole;
    use crate::model::ModelCapabilities;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn test_conversation_new() {
        let cap = ContextCap::new(1000);
        let capabilities = ModelCapabilities::default();
        let conv = Conversation::new("System prompt".into(), cap, capabilities);
        assert_eq!(conv.system_prompt(), "System prompt");
        assert_eq!(conv.messages().len(), 0);
        assert!(conv.estimated_tokens() > 0); // system prompt tokens
    }

    #[test]
    fn test_conversation_from_tier() {
        let conv = Conversation::from_tier(ModelTier::Small, 4096);
        assert!(conv.system_prompt().contains("coding agent"));
        assert_eq!(conv.capabilities().tier, ModelTier::Small);
    }

    #[test]
    fn test_add_user_message() {
        let mut conv = Conversation::from_tier(ModelTier::Small, 4096);
        conv.add_user_message("Hello".into()).unwrap();
        assert_eq!(conv.messages().len(), 1);
        assert_eq!(conv.messages()[0].role, MessageRole::User);
        assert_eq!(conv.messages()[0].content, "Hello");
    }

    #[test]
    fn test_add_assistant_message() {
        let mut conv = Conversation::from_tier(ModelTier::Small, 4096);
        conv.add_assistant_message("Hi there".into(), None, None)
            .unwrap();
        assert_eq!(conv.messages().len(), 1);
        assert_eq!(conv.messages()[0].role, MessageRole::Assistant);
    }

    #[test]
    fn test_add_tool_message() {
        let mut conv = Conversation::from_tier(ModelTier::Small, 4096);
        conv.add_tool_message("Tool result".into(), "call-123".into())
            .unwrap();
        assert_eq!(conv.messages().len(), 1);
        assert_eq!(conv.messages()[0].role, MessageRole::Tool);
        assert_eq!(conv.messages()[0].tool_call_id, Some("call-123".into()));
    }

    #[test]
    fn test_to_model_messages() {
        let mut conv = Conversation::from_tier(ModelTier::Small, 4096);
        conv.add_user_message("Hello".into()).unwrap();
        conv.add_assistant_message("Hi".into(), None, None).unwrap();

        let msgs = conv.to_model_messages();
        assert_eq!(msgs.len(), 3); // system + user + assistant
        assert_eq!(msgs[0].role, crate::model::ModelRole::System);
        assert_eq!(msgs[1].role, crate::model::ModelRole::User);
        assert_eq!(msgs[2].role, crate::model::ModelRole::Assistant);
    }

    #[test]
    fn test_history_is_never_windowed() {
        let mut conv = Conversation::from_tier(ModelTier::Small, 100_000);
        for i in 0..40 {
            conv.add_user_message(format!("question {i}")).unwrap();
            conv.add_assistant_message(format!("answer {i}"), None, None)
                .unwrap();
        }
        assert_eq!(conv.to_model_messages().len(), 81);
    }

    #[test]
    fn test_context_cap_enforcement() {
        let mut conv = Conversation::from_tier(ModelTier::Small, 100); // Very small cap
        // Fill up to near capacity
        conv.add_user_message("A".repeat(200)).unwrap_err(); // Should fail - too large
    }

    #[test]
    fn test_load_skills() {
        let dir = tempdir().unwrap();
        let skills_dir = dir.path();

        // Create skill files
        fs::write(skills_dir.join("skill1.md"), "Skill 1 content").unwrap();
        fs::write(skills_dir.join("skill2.md"), "Skill 2 content").unwrap();
        fs::write(skills_dir.join("not-a-skill.txt"), "Ignored").unwrap();

        let skills = load_skills(skills_dir).unwrap();
        assert!(skills.contains("Skill 1 content"));
        assert!(skills.contains("Skill 2 content"));
        assert!(!skills.contains("not-a-skill"));
    }

    #[test]
    fn test_load_skills_empty_dir() {
        let dir = tempdir().unwrap();
        let skills = load_skills(dir.path()).unwrap();
        assert_eq!(skills, "");
    }

    #[test]
    fn test_load_skills_nonexistent() {
        let result = load_skills(Path::new("/nonexistent/path"));
        assert!(result.is_err());
    }

    #[test]
    fn test_build_system_prompt_with_skills() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("test.md"), "Test skill").unwrap();

        let prompt = build_system_prompt(ModelTier::Small, Some(dir.path())).unwrap();
        assert!(prompt.contains("coding agent")); // base prompt
        assert!(prompt.contains("Test skill")); // skill content
        assert!(prompt.contains("## Skills")); // section header
    }

    #[test]
    fn test_clear_history() {
        let mut conv = Conversation::from_tier(ModelTier::Small, 4096);
        conv.add_user_message("Hello".into()).unwrap();
        conv.add_assistant_message("Hi".into(), None, None).unwrap();

        let tokens_before = conv.estimated_tokens();
        conv.clear_history();

        assert_eq!(conv.messages().len(), 0);
        assert_eq!(
            conv.estimated_tokens(),
            conv.token_counter
                .count_message("system", conv.system_prompt())
        );
        assert!(conv.estimated_tokens() < tokens_before);
    }

    #[test]
    fn test_record_usage_anchors_and_calibrates() {
        let mut conv = Conversation::from_tier(ModelTier::Frontier, 100_000);
        conv.add_user_message("hello world".into()).unwrap();
        conv.add_assistant_message("hi".into(), None, None).unwrap();

        conv.record_usage(9_000, 100);
        assert_eq!(conv.estimated_tokens(), 9_100);
        assert!(conv.calibration_ratio() > 1.0);

        // Only the new message is estimated on top of the real count.
        let before = conv.estimated_tokens();
        conv.add_user_message("x".repeat(400)).unwrap();
        let added = conv.estimated_tokens() - before;
        assert!(added >= 100, "added={added}");
        assert!(conv.estimated_tokens() < before + 2_000);
    }

    #[test]
    fn test_record_usage_ignores_missing_usage() {
        let mut conv = Conversation::from_tier(ModelTier::Frontier, 100_000);
        conv.add_user_message("hello".into()).unwrap();
        let before = conv.estimated_tokens();
        conv.record_usage(0, 0);
        assert_eq!(conv.estimated_tokens(), before);
        assert_eq!(conv.calibration_ratio(), 1.0);
    }

    #[test]
    fn test_calibration_survives_history_rewrite() {
        let mut conv = Conversation::from_tier(ModelTier::Frontier, 100_000);
        conv.add_user_message("hello world".into()).unwrap();
        conv.record_usage(9_000, 100);
        let ratio = conv.calibration_ratio();
        let gen_before = conv.generation();
        conv.replace_messages(Vec::new());
        assert_eq!(conv.calibration_ratio(), ratio);
        assert_eq!(conv.generation(), gen_before + 1);
        assert!(conv.estimated_tokens() > conv.token_counter.count("x"));
    }

    fn conv_with_tool_results(sizes: &[usize]) -> Conversation {
        let mut conv = Conversation::from_tier(ModelTier::Frontier, 1_000_000);
        for (i, size) in sizes.iter().enumerate() {
            conv.add_assistant_message(
                String::new(),
                Some(vec![crate::events::ToolCall {
                    id: format!("c{i}"),
                    name: "run".into(),
                    args: serde_json::json!({"cmd": "x"}),
                    status: crate::events::ToolCallStatus::Pending,
                }]),
                None,
            )
            .unwrap();
            conv.add_tool_message("o".repeat(*size), format!("c{i}"))
                .unwrap();
        }
        conv
    }

    #[test]
    fn test_prune_keeps_recent_and_preserves_pairing() {
        let mut conv = conv_with_tool_results(&[4_000, 4_000, 4_000, 4_000]);
        let count = conv.message_count();
        let before = conv.estimated_tokens();

        let stats = conv.prune_tool_results(500, 100).unwrap();
        assert_eq!(stats.messages_pruned, 3);
        assert_eq!(conv.message_count(), count);
        assert!(conv.estimated_tokens() < before);

        let tools: Vec<_> = conv
            .messages()
            .iter()
            .filter(|m| m.role == MessageRole::Tool)
            .collect();
        assert!(
            tools[..3]
                .iter()
                .all(|m| m.content.starts_with(PRUNED_TOOL_OUTPUT_PREFIX))
        );
        assert_eq!(tools[3].content.len(), 4_000);
        assert_eq!(tools[0].tool_call_id.as_deref(), Some("c0"));
    }

    #[test]
    fn test_prune_is_idempotent_and_respects_min_savings() {
        let mut conv = conv_with_tool_results(&[4_000, 4_000, 4_000]);
        assert!(conv.prune_tool_results(1_500, 1_000_000).is_none());
        assert!(conv.prune_tool_results(1_500, 100).is_some());
        assert!(conv.prune_tool_results(1_500, 1).is_none());
    }
}
