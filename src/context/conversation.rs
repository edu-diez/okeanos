//! Conversation context management.
//!
//! Handles the system prompt, skills directory concatenation,
//! message history, and context cap enforcement.

use crate::context::cap::{ContextCap, ContextCapError, ContextWarning};
use crate::context::tokens::{CostEstimate, TokenCounter};
use crate::events::Message;
use crate::model::{ModelCapabilities, ModelTier};
use std::path::{Path, PathBuf};
use thiserror::Error;

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
    /// Total estimated tokens in context (system + messages).
    estimated_tokens: usize,
    /// Current cost estimate.
    cost_estimate: CostEstimate,
}

impl Conversation {
    /// Create a new conversation with the given system prompt and context cap.
    pub fn new(system_prompt: String, cap: ContextCap, capabilities: ModelCapabilities) -> Self {
        let token_counter = TokenCounter;
        let system_tokens = token_counter.count_message("system", &system_prompt);

        Self {
            system_prompt,
            messages: Vec::new(),
            cap,
            token_counter,
            capabilities,
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

    /// Get the current estimated token count.
    pub fn estimated_tokens(&self) -> usize {
        self.estimated_tokens
    }

    /// Get the current cost estimate.
    pub fn cost_estimate(&self) -> &CostEstimate {
        &self.cost_estimate
    }

    /// Get the context warning level.
    pub fn warning_level(&self) -> ContextWarning {
        ContextWarning::from_tokens(&self.cap, self.estimated_tokens)
    }

    /// Get remaining tokens before cap.
    pub fn remaining_tokens(&self) -> usize {
        self.cap.remaining(self.estimated_tokens)
    }

    /// Add a user message.
    pub fn add_user_message(&mut self, content: String) -> Result<(), ConversationError> {
        let msg = Message {
            role: crate::events::MessageRole::User,
            content,
            tool_calls: None,
            tool_call_id: None,
            ts: current_timestamp(),
        };
        self.add_message(msg)
    }

    /// Add an assistant message.
    pub fn add_assistant_message(
        &mut self,
        content: String,
        tool_calls: Option<Vec<crate::events::ToolCall>>,
    ) -> Result<(), ConversationError> {
        let msg = Message {
            role: crate::events::MessageRole::Assistant,
            content,
            tool_calls,
            tool_call_id: None,
            ts: current_timestamp(),
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
            role: crate::events::MessageRole::Tool,
            content,
            tool_calls: None,
            tool_call_id: Some(tool_call_id),
            ts: current_timestamp(),
        };
        self.add_message(msg)
    }

    /// Add a message with cap enforcement.
    fn add_message(&mut self, msg: Message) -> Result<(), ConversationError> {
        let msg_tokens = self
            .token_counter
            .count_message(&msg.role.to_string(), &msg.content);

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
        let msg_tokens = self
            .token_counter
            .count_message(&msg.role.to_string(), &msg.content);
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

    /// Get all messages including system prompt as ModelMessages.
    pub fn to_model_messages(&self) -> Vec<crate::model::ModelMessage> {
        let mut result = Vec::with_capacity(self.messages.len() + 1);

        // System prompt first
        result.push(crate::model::ModelMessage {
            role: crate::model::ModelRole::System,
            content: self.system_prompt.clone(),
            tool_calls: None,
            tool_call_id: None,
        });

        // Then conversation history
        for msg in &self.messages {
            result.push(crate::model::ModelMessage {
                role: match msg.role {
                    crate::events::MessageRole::User => crate::model::ModelRole::User,
                    crate::events::MessageRole::Assistant => crate::model::ModelRole::Assistant,
                    crate::events::MessageRole::Tool => crate::model::ModelRole::Tool,
                    crate::events::MessageRole::System => crate::model::ModelRole::System,
                },
                content: msg.content.clone(),
                tool_calls: msg.tool_calls.clone(),
                tool_call_id: msg.tool_call_id.clone(),
            });
        }

        result
    }

    /// Clear history but keep system prompt.
    pub fn clear_history(&mut self) {
        self.messages.clear();
        self.estimated_tokens = self
            .token_counter
            .count_message("system", &self.system_prompt);
        self.cost_estimate = CostEstimate::default();
    }

    /// Get the number of messages in history (excluding system).
    pub fn message_count(&self) -> usize {
        self.messages.len()
    }
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

/// Get the built-in system prompt for a tier.
fn system_prompt_for_tier(tier: ModelTier) -> &'static str {
    match tier {
        ModelTier::Small => include_str!("../../assets/prompts/small_system_prompt.txt"),
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
        conv.add_assistant_message("Hi there".into(), None).unwrap();
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
        conv.add_assistant_message("Hi".into(), None).unwrap();

        let msgs = conv.to_model_messages();
        assert_eq!(msgs.len(), 3); // system + user + assistant
        assert_eq!(msgs[0].role, crate::model::ModelRole::System);
        assert_eq!(msgs[1].role, crate::model::ModelRole::User);
        assert_eq!(msgs[2].role, crate::model::ModelRole::Assistant);
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
        conv.add_assistant_message("Hi".into(), None).unwrap();

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
}
