//! Approximate token counting and cost estimation.

use std::collections::HashMap;

/// Approximate token counter using a simple character-based heuristic.
/// This is an estimate, not provider-authoritative.
#[derive(Debug, Clone, Copy, Default)]
pub struct TokenCounter;

impl TokenCounter {
    /// Estimate tokens from text.
    /// Rough approximation: ~4 characters per token for English text.
    pub fn count(&self, text: &str) -> usize {
        if text.is_empty() {
            return 0;
        }
        // Simple heuristic: ~4 chars per token
        // Add a small overhead for message formatting
        text.len().div_ceil(4)
    }

    /// Estimate tokens for a conversation message.
    pub fn count_message(&self, role: &str, content: &str) -> usize {
        let role_tokens = match role {
            "system" => 10,
            "user" => 5,
            "assistant" => 5,
            "tool" => 5,
            _ => 5,
        };
        role_tokens + self.count(content)
    }

    /// Estimate tokens for a list of messages.
    pub fn count_messages(&self, messages: &[(String, String)]) -> usize {
        messages
            .iter()
            .map(|(role, content)| self.count_message(role, content))
            .sum()
    }
}

/// Cost estimate for a model interaction.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct CostEstimate {
    pub input_tokens: usize,
    pub output_tokens: usize,
    pub input_cost_usd: f64,
    pub output_cost_usd: f64,
}

impl CostEstimate {
    pub fn total_cost_usd(&self) -> f64 {
        self.input_cost_usd + self.output_cost_usd
    }

    pub fn total_tokens(&self) -> usize {
        self.input_tokens + self.output_tokens
    }
}

/// Per-model pricing (USD per 1M tokens).
/// These are estimates and should be configured per-model.
#[derive(Debug, Clone)]
pub struct ModelPricing {
    pub input_per_million: f64,
    pub output_per_million: f64,
}

impl Default for ModelPricing {
    fn default() -> Self {
        Self {
            input_per_million: 0.0,
            output_per_million: 0.0,
        }
    }
}

/// Pricing table for known models.
pub fn default_pricing_table() -> HashMap<String, ModelPricing> {
    let mut table = HashMap::new();
    // OpenAI models (approximate)
    table.insert(
        "gpt-4o".to_string(),
        ModelPricing {
            input_per_million: 2.50,
            output_per_million: 10.00,
        },
    );
    table.insert(
        "gpt-4o-mini".to_string(),
        ModelPricing {
            input_per_million: 0.15,
            output_per_million: 0.60,
        },
    );
    table.insert(
        "gpt-4-turbo".to_string(),
        ModelPricing {
            input_per_million: 10.00,
            output_per_million: 30.00,
        },
    );
    table.insert(
        "gpt-3.5-turbo".to_string(),
        ModelPricing {
            input_per_million: 0.50,
            output_per_million: 1.50,
        },
    );
    // Anthropic models (approximate)
    table.insert(
        "claude-3-5-sonnet".to_string(),
        ModelPricing {
            input_per_million: 3.00,
            output_per_million: 15.00,
        },
    );
    table.insert(
        "claude-3-haiku".to_string(),
        ModelPricing {
            input_per_million: 0.25,
            output_per_million: 1.25,
        },
    );
    table
}

/// Estimate cost for a model interaction.
pub fn estimate_cost(
    model_id: &str,
    input_tokens: usize,
    output_tokens: usize,
    pricing_table: &HashMap<String, ModelPricing>,
) -> CostEstimate {
    let pricing = pricing_table.get(model_id).cloned().unwrap_or_default();

    let input_cost_usd = (input_tokens as f64 / 1_000_000.0) * pricing.input_per_million;
    let output_cost_usd = (output_tokens as f64 / 1_000_000.0) * pricing.output_per_million;

    CostEstimate {
        input_tokens,
        output_tokens,
        input_cost_usd,
        output_cost_usd,
    }
}

/// Trait for getting pricing; allows custom pricing per model.
pub trait PricingProvider {
    fn pricing(&self, model_id: &str) -> ModelPricing;
}

impl PricingProvider for HashMap<String, ModelPricing> {
    fn pricing(&self, model_id: &str) -> ModelPricing {
        self.get(model_id).cloned().unwrap_or_default()
    }
}
