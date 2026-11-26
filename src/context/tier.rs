//! Model tier configuration — maps tier to prompt template, tool subset, and generation parameters.

use crate::context::selective::SelectiveContextConfig;
use crate::model::{GenerationParams, ModelCapabilities, ModelTier};
use std::collections::HashSet;

/// Configuration derived from model tier.
#[derive(Debug, Clone)]
pub struct TierConfig {
    /// The model tier.
    pub tier: ModelTier,
    /// System prompt template for this tier.
    pub system_prompt: &'static str,
    /// Tool names available to this tier.
    pub allowed_tools: HashSet<String>,
    /// Default generation parameters for this tier.
    pub generation_params: GenerationParams,
    /// Selective context configuration.
    pub selective_context: SelectiveContextConfig,
    /// Whether to include thinking channel in context (replayed for frontier, not for small).
    pub replay_thinking: bool,
}

impl TierConfig {
    /// Get the tier configuration for a given tier.
    pub fn for_tier(tier: ModelTier) -> Self {
        match tier {
            ModelTier::Small => Self::small(),
            ModelTier::Mid => Self::mid(),
            ModelTier::Frontier => Self::frontier(),
        }
    }

    /// Small model tier configuration (4k-32k context, limited/no native function calling).
    fn small() -> Self {
        let mut allowed_tools = HashSet::new();
        // Small models get minimal tool set: read, edit, run
        allowed_tools.insert("read".to_string());
        allowed_tools.insert("edit".to_string());
        allowed_tools.insert("run".to_string());

        Self {
            tier: ModelTier::Small,
            system_prompt: include_str!("../../assets/prompts/small_system_prompt.txt"),
            allowed_tools,
            generation_params: GenerationParams {
                temperature: Some(0.2),
                max_tokens: Some(2048),
                stop_sequences: None,
                top_p: Some(0.9),
                ..Default::default()
            },
            selective_context: SelectiveContextConfig {
                max_files: 5,
                max_tokens_per_file: 1000,
                ..Default::default()
            },
            replay_thinking: false,
        }
    }

    /// Mid-tier model tier configuration (32k-128k context, native function calling).
    fn mid() -> Self {
        let mut allowed_tools = HashSet::new();
        // Mid models get full set: read, write, delete, run, grep, todo
        allowed_tools.insert("todo".to_string());
        allowed_tools.insert("read".to_string());
        allowed_tools.insert("write".to_string());
        allowed_tools.insert("delete".to_string());
        allowed_tools.insert("run".to_string());
        allowed_tools.insert("grep".to_string());

        Self {
            tier: ModelTier::Mid,
            system_prompt: include_str!("../../assets/prompts/mid_system_prompt.txt"),
            allowed_tools,
            generation_params: GenerationParams {
                temperature: Some(0.5),
                max_tokens: Some(4096),
                stop_sequences: None,
                top_p: Some(0.95),
                ..Default::default()
            },
            selective_context: SelectiveContextConfig {
                max_files: 10,
                max_tokens_per_file: 2000,
                ..Default::default()
            },
            replay_thinking: false,
        }
    }

    /// Frontier model tier configuration (128k+ context, full capabilities).
    fn frontier() -> Self {
        let mut allowed_tools = HashSet::new();
        // Frontier models get full set: read, write, delete, run, grep, todo
        allowed_tools.insert("todo".to_string());
        allowed_tools.insert("read".to_string());
        allowed_tools.insert("write".to_string());
        allowed_tools.insert("delete".to_string());
        allowed_tools.insert("run".to_string());
        allowed_tools.insert("grep".to_string());

        Self {
            tier: ModelTier::Frontier,
            system_prompt: include_str!("../../assets/prompts/frontier_system_prompt.txt"),
            allowed_tools,
            generation_params: GenerationParams {
                temperature: Some(0.7),
                max_tokens: Some(8192),
                stop_sequences: None,
                top_p: Some(0.95),
                ..Default::default()
            },
            selective_context: SelectiveContextConfig {
                max_files: 20,
                max_tokens_per_file: 3000,
                ..Default::default()
            },
            replay_thinking: true,
        }
    }

    /// Create a tier config from model capabilities (allows override from config).
    pub fn from_capabilities(capabilities: &ModelCapabilities) -> Self {
        let mut config = Self::for_tier(capabilities.tier);

        // Override generation params if specified in capabilities
        if capabilities.temperature_supported {
            // Temperature is already set per tier, but could be overridden per-model
        }
        if capabilities.max_output > 0 {
            config.generation_params.max_tokens = Some(capabilities.max_output.min(8192));
        }

        config
    }

    /// Check if a tool is allowed for this tier.
    pub fn allows_tool(&self, tool_name: &str) -> bool {
        self.allowed_tools.contains(tool_name)
    }

    /// Get the filtered tool names for this tier.
    pub fn allowed_tool_names(&self) -> Vec<String> {
        self.allowed_tools.iter().cloned().collect()
    }
}

/// Tool subsets per tier for easy reference.
pub mod tool_subsets {
    use std::collections::HashSet;

    pub fn small() -> HashSet<String> {
        let mut set = HashSet::new();
        set.insert("read".to_string());
        set.insert("edit".to_string());
        set.insert("run".to_string());
        set
    }

    pub fn mid() -> HashSet<String> {
        let mut set = HashSet::new();
        set.insert("todo".to_string());
        set.insert("read".to_string());
        set.insert("write".to_string());
        set.insert("delete".to_string());
        set.insert("run".to_string());
        set.insert("grep".to_string());
        set
    }

    pub fn frontier() -> HashSet<String> {
        let mut set = HashSet::new();
        set.insert("todo".to_string());
        set.insert("read".to_string());
        set.insert("write".to_string());
        set.insert("delete".to_string());
        set.insert("run".to_string());
        set.insert("grep".to_string());
        set
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ModelCapabilities;

    #[test]
    fn test_tier_config_small() {
        let config = TierConfig::for_tier(ModelTier::Small);
        assert_eq!(config.tier, ModelTier::Small);
        assert!(!config.allows_tool("todo"));
        assert!(config.allows_tool("read"));
        assert!(!config.allows_tool("write"));
        assert!(config.allows_tool("edit"));
        assert!(config.allows_tool("run"));
        assert!(!config.allows_tool("delete"));
        assert!(!config.allows_tool("grep"));
        assert_eq!(config.generation_params.temperature, Some(0.2));
        assert_eq!(config.generation_params.max_tokens, Some(2048));
        assert!(!config.replay_thinking);
    }

    #[test]
    fn test_tier_config_mid() {
        let config = TierConfig::for_tier(ModelTier::Mid);
        assert_eq!(config.tier, ModelTier::Mid);
        assert!(config.allows_tool("todo"));
        assert!(config.allows_tool("read"));
        assert!(config.allows_tool("write"));
        assert!(!config.allows_tool("edit"));
        assert!(config.allows_tool("delete"));
        assert!(config.allows_tool("run"));
        assert!(config.allows_tool("grep"));
        assert_eq!(config.generation_params.temperature, Some(0.5));
        assert_eq!(config.generation_params.max_tokens, Some(4096));
        assert!(!config.replay_thinking);
    }

    #[test]
    fn test_tier_config_frontier() {
        let config = TierConfig::for_tier(ModelTier::Frontier);
        assert_eq!(config.tier, ModelTier::Frontier);
        assert!(config.allows_tool("todo"));
        assert!(config.allows_tool("read"));
        assert!(config.allows_tool("write"));
        assert!(!config.allows_tool("edit"));
        assert!(config.allows_tool("delete"));
        assert!(config.allows_tool("run"));
        assert!(config.allows_tool("grep"));
        assert_eq!(config.generation_params.temperature, Some(0.7));
        assert_eq!(config.generation_params.max_tokens, Some(8192));
        assert!(config.replay_thinking);
    }

    #[test]
    fn test_tier_config_from_capabilities() {
        let capabilities = ModelCapabilities {
            tier: ModelTier::Small,
            max_output: 1024,
            temperature_supported: true,
            ..Default::default()
        };
        let config = TierConfig::from_capabilities(&capabilities);
        assert_eq!(config.generation_params.max_tokens, Some(1024));
    }

    #[test]
    fn test_tool_subsets() {
        let small = tool_subsets::small();
        let mid = tool_subsets::mid();
        let frontier = tool_subsets::frontier();

        assert!(!small.contains("todo"));
        assert!(small.contains("read"));
        assert!(small.contains("edit"));
        assert!(small.contains("run"));
        assert!(!small.contains("delete"));
        assert!(!small.contains("grep"));
        assert!(!small.contains("write"));

        assert!(mid.contains("todo"));
        assert!(mid.contains("read"));
        assert!(!mid.contains("edit"));
        assert!(mid.contains("write"));
        assert!(mid.contains("delete"));
        assert!(mid.contains("run"));
        assert!(mid.contains("grep"));

        assert!(frontier.contains("todo"));
        assert!(frontier.contains("read"));
        assert!(!frontier.contains("edit"));
        assert!(frontier.contains("write"));
        assert!(frontier.contains("delete"));
        assert!(frontier.contains("run"));
        assert!(frontier.contains("grep"));
    }
}
