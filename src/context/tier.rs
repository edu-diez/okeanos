//! Model tier configuration — generation parameters, selective-context limits,
//! thinking replay and tool output caps per tier.
//!
//! Which tools a tier sees is decided only by [`crate::tool::builtin_registry`];
//! system prompts live in `assets/prompts/` and never list tools.

use crate::context::selective::SelectiveContextConfig;
use crate::model::{GenerationParams, ModelCapabilities, ModelTier};

/// Configuration derived from model tier.
#[derive(Debug, Clone)]
pub struct TierConfig {
    /// The model tier.
    pub tier: ModelTier,
    /// Default generation parameters for this tier.
    pub generation_params: GenerationParams,
    /// Selective context configuration.
    pub selective_context: SelectiveContextConfig,
    /// Whether to include thinking channel in context (replayed for frontier, not for small).
    pub replay_thinking: bool,
    /// Maximum bytes of a tool result shown to the model (head/tail truncated).
    pub tool_output_cap: usize,
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
        Self {
            tier: ModelTier::Small,
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
            tool_output_cap: 4_000,
        }
    }

    /// Mid-tier model tier configuration (32k-128k context, native function calling).
    fn mid() -> Self {
        Self {
            tier: ModelTier::Mid,
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
            tool_output_cap: 10_000,
        }
    }

    /// Frontier model tier configuration (128k+ context, full capabilities).
    fn frontier() -> Self {
        Self {
            tier: ModelTier::Frontier,
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
            tool_output_cap: 20_000,
        }
    }

    /// Create a tier config from model capabilities (allows override from config).
    pub fn from_capabilities(capabilities: &ModelCapabilities) -> Self {
        let mut config = Self::for_tier(capabilities.tier);
        if capabilities.max_output > 0 {
            config.generation_params.max_tokens = Some(capabilities.max_output.min(8192));
        }
        config
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
        assert_eq!(config.generation_params.temperature, Some(0.2));
        assert_eq!(config.generation_params.max_tokens, Some(2048));
        assert!(!config.replay_thinking);
    }

    #[test]
    fn test_tier_config_mid() {
        let config = TierConfig::for_tier(ModelTier::Mid);
        assert_eq!(config.tier, ModelTier::Mid);
        assert_eq!(config.generation_params.temperature, Some(0.5));
        assert_eq!(config.generation_params.max_tokens, Some(4096));
        assert!(!config.replay_thinking);
    }

    #[test]
    fn test_tier_config_frontier() {
        let config = TierConfig::for_tier(ModelTier::Frontier);
        assert_eq!(config.tier, ModelTier::Frontier);
        assert_eq!(config.generation_params.temperature, Some(0.7));
        assert_eq!(config.generation_params.max_tokens, Some(8192));
        assert!(config.replay_thinking);
    }

    #[test]
    fn tool_output_cap_grows_with_tier() {
        let cap = |t| TierConfig::for_tier(t).tool_output_cap;
        assert!(cap(ModelTier::Small) < cap(ModelTier::Mid));
        assert!(cap(ModelTier::Mid) < cap(ModelTier::Frontier));
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
}
