//! Cost budget enforcement for session-level spending caps.
//!
//! Provides opt-in hard session cost cap (USD) + per-turn soft warning threshold.
//! When cap reached, refuse new tool calls that would exceed it; let in-flight work finish.
//! Fail closed ($0 + warning) on unknown pricing.

use crate::context::tokens::{CostEstimate, ModelPricing, PricingProvider};
use crate::events::{EventSender, LogLevel};
use crate::model::ModelCapabilities;
use std::fmt;
use std::sync::Arc;
use thiserror::Error;

/// Errors from cost budget enforcement.
#[derive(Debug, Error)]
pub enum CostBudgetError {
    #[error("session cost cap exceeded: ${spent:.4} / ${cap:.4} (projected: ${projected:.4})")]
    CapExceeded {
        spent: f64,
        cap: f64,
        projected: f64,
    },
    #[error("unknown pricing for model '{model_id}': refusing to execute (fail closed)")]
    UnknownPricing { model_id: String },
    #[error("per-turn cost warning: ${cost:.4} exceeds threshold ${threshold:.4}")]
    PerTurnWarning { cost: f64, threshold: f64 },
}

/// Configuration for cost budget.
#[derive(Debug, Clone, Copy, Default)]
pub struct CostBudgetConfig {
    /// Hard session cost cap in USD. None = disabled (default at MVP).
    pub session_cap_usd: Option<f64>,
    /// Soft warning threshold per turn in USD. None = disabled.
    pub per_turn_warning_usd: Option<f64>,
    /// Confirmation threshold for expensive tool calls in USD. None = disabled.
    /// When a tool call's estimated cost exceeds this threshold, user confirmation is required.
    pub expensive_call_confirmation_usd: Option<f64>,
}

impl CostBudgetConfig {
    /// Create a new cost budget config from the config section.
    pub fn from_config_section(section: &crate::config::CostSection) -> Self {
        Self {
            session_cap_usd: section.session_cap_usd,
            per_turn_warning_usd: section.per_turn_warning_usd,
            expensive_call_confirmation_usd: section.expensive_call_confirmation_usd,
        }
    }

    /// Check if cost budget is enabled (has any limit).
    pub fn is_enabled(&self) -> bool {
        self.session_cap_usd.is_some()
            || self.per_turn_warning_usd.is_some()
            || self.expensive_call_confirmation_usd.is_some()
    }

    /// Get the expensive call confirmation threshold.
    pub fn expensive_call_confirmation_usd(&self) -> Option<f64> {
        self.expensive_call_confirmation_usd
    }
}

/// Trait for providing pricing information for models.
pub trait PricingProviderExt: PricingProvider {
    /// Get pricing for a model, returning None if unknown (fail closed).
    fn try_pricing(&self, model_id: &str) -> Option<ModelPricing> {
        let pricing = self.pricing(model_id);
        if pricing.input_per_million == 0.0 && pricing.output_per_million == 0.0 {
            None
        } else {
            Some(pricing)
        }
    }
}

impl<T: ?Sized + PricingProvider> PricingProviderExt for T {}

/// Tracks cost accumulation and enforces budget limits.
pub struct CostTracker {
    /// Total spent so far in USD.
    spent_usd: f64,
    /// Configuration for the budget.
    config: CostBudgetConfig,
    /// Model ID for pricing lookups.
    model_id: String,
    /// Pricing provider for cost estimation.
    pricing: Arc<dyn PricingProvider + Send + Sync>,
    /// Model capabilities (for tier-aware behavior).
    capabilities: ModelCapabilities,
}

impl fmt::Debug for CostTracker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CostTracker")
            .field("spent_usd", &self.spent_usd)
            .field("config", &self.config)
            .field("model_id", &self.model_id)
            .field("capabilities", &self.capabilities)
            .field("pricing", &"<dyn PricingProvider>")
            .finish()
    }
}

impl Clone for CostTracker {
    fn clone(&self) -> Self {
        Self {
            spent_usd: self.spent_usd,
            config: self.config,
            model_id: self.model_id.clone(),
            pricing: Arc::clone(&self.pricing),
            capabilities: self.capabilities.clone(),
        }
    }
}

impl CostTracker {
    /// Create a new cost tracker.
    pub fn new(
        config: CostBudgetConfig,
        model_id: String,
        pricing: Arc<dyn PricingProvider + Send + Sync>,
        capabilities: ModelCapabilities,
    ) -> Self {
        Self {
            spent_usd: 0.0,
            config,
            model_id,
            pricing,
            capabilities,
        }
    }

    /// Get the current total spent.
    pub fn spent_usd(&self) -> f64 {
        self.spent_usd
    }

    /// Get the session cap if configured.
    pub fn session_cap_usd(&self) -> Option<f64> {
        self.config.session_cap_usd
    }

    /// Get the per-turn warning threshold if configured.
    pub fn per_turn_warning_usd(&self) -> Option<f64> {
        self.config.per_turn_warning_usd
    }

    /// Get the expensive call confirmation threshold if configured.
    pub fn expensive_call_confirmation_usd(&self) -> Option<f64> {
        self.config.expensive_call_confirmation_usd
    }

    /// Check if a tool call's estimated cost exceeds the expensive call confirmation threshold.
    ///
    /// Returns true if confirmation is required (cost exceeds threshold).
    pub fn requires_expensive_call_confirmation(&self, projected_cost: f64) -> bool {
        self.config
            .expensive_call_confirmation_usd
            .is_some_and(|threshold| projected_cost > threshold)
    }

    /// Get the model ID.
    pub fn model_id(&self) -> &str {
        &self.model_id
    }

    /// Get the model capabilities.
    pub fn capabilities(&self) -> &ModelCapabilities {
        &self.capabilities
    }

    /// Get the pricing provider.
    pub fn pricing(&self) -> &Arc<dyn PricingProvider + Send + Sync> {
        &self.pricing
    }

    /// Check if the budget is enabled.
    pub fn is_enabled(&self) -> bool {
        self.config.is_enabled()
    }

    /// Estimate the cost of a tool call execution.
    ///
    /// Returns the estimated cost in USD, or an error if pricing is unknown.
    pub fn estimate_tool_call_cost(
        &self,
        input_tokens: usize,
        output_tokens: usize,
    ) -> Result<CostEstimate, CostBudgetError> {
        let pricing = self.pricing.try_pricing(&self.model_id).ok_or_else(|| {
            CostBudgetError::UnknownPricing {
                model_id: self.model_id.clone(),
            }
        })?;

        let input_cost_usd = (input_tokens as f64 / 1_000_000.0) * pricing.input_per_million;
        let output_cost_usd = (output_tokens as f64 / 1_000_000.0) * pricing.output_per_million;

        Ok(CostEstimate {
            input_tokens,
            output_tokens,
            input_cost_usd,
            output_cost_usd,
        })
    }

    /// Check if a projected cost would exceed the session cap.
    ///
    /// Returns an error if the cap would be exceeded.
    pub fn check_session_cap(&self, projected_cost: f64) -> Result<(), CostBudgetError> {
        if let Some(cap) = self.config.session_cap_usd {
            let projected_total = self.spent_usd + projected_cost;
            if projected_total > cap {
                return Err(CostBudgetError::CapExceeded {
                    spent: self.spent_usd,
                    cap,
                    projected: projected_total,
                });
            }
        }
        Ok(())
    }

    /// Check if a projected per-turn cost exceeds the warning threshold.
    ///
    /// Emits a warning event if threshold is exceeded. Does not block execution.
    pub async fn check_per_turn_warning(&self, projected_cost: f64, event_tx: &EventSender) {
        if let Some(threshold) = self.config.per_turn_warning_usd
            && projected_cost > threshold
        {
            let warning = CostBudgetError::PerTurnWarning {
                cost: projected_cost,
                threshold,
            };
            let event = crate::events::Event::Log(crate::events::Log {
                level: LogLevel::Warn,
                message: warning.to_string(),
            });
            let _ = event_tx.send(event);
        }
    }

    /// Record a completed tool call's actual cost.
    ///
    /// Updates the spent total and returns the new total.
    pub fn record_cost(&mut self, cost: &CostEstimate) -> f64 {
        self.spent_usd += cost.total_cost_usd();
        self.spent_usd
    }

    /// Record cost from a model response usage.
    pub fn record_usage(&mut self, usage: &crate::model::Usage) -> f64 {
        let cost = self
            .estimate_tool_call_cost(usage.prompt_tokens, usage.completion_tokens)
            .unwrap_or_else(|_| CostEstimate::default());
        self.record_cost(&cost)
    }

    /// Get a summary of the current budget state.
    pub fn summary(&self) -> CostBudgetSummary {
        CostBudgetSummary {
            spent_usd: self.spent_usd,
            session_cap_usd: self.config.session_cap_usd,
            per_turn_warning_usd: self.config.per_turn_warning_usd,
            expensive_call_confirmation_usd: self.config.expensive_call_confirmation_usd,
            model_id: self.model_id.clone(),
            remaining_usd: self.config.session_cap_usd.map(|cap| cap - self.spent_usd),
        }
    }
}

/// Summary of cost budget state for status display.
#[derive(Debug, Clone)]
pub struct CostBudgetSummary {
    pub spent_usd: f64,
    pub session_cap_usd: Option<f64>,
    pub per_turn_warning_usd: Option<f64>,
    pub expensive_call_confirmation_usd: Option<f64>,
    pub model_id: String,
    pub remaining_usd: Option<f64>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::tokens::{ModelPricing, default_pricing_table};
    use crate::model::ModelCapabilities;
    use std::collections::HashMap;
    use std::sync::Arc;

    struct TestPricingProvider {
        table: HashMap<String, ModelPricing>,
    }

    impl PricingProvider for TestPricingProvider {
        fn pricing(&self, model_id: &str) -> ModelPricing {
            self.table.get(model_id).cloned().unwrap_or_default()
        }
    }

    #[test]
    fn test_cost_tracker_disabled_by_default() {
        let config = CostBudgetConfig::default();
        let pricing = Arc::new(TestPricingProvider {
            table: default_pricing_table(),
        });
        let tracker = CostTracker::new(
            config,
            "gpt-4o".to_string(),
            pricing,
            ModelCapabilities::default(),
        );
        assert!(!tracker.is_enabled());
    }

    #[test]
    fn test_cost_tracker_enabled_with_session_cap() {
        let config = CostBudgetConfig {
            session_cap_usd: Some(10.0),
            ..Default::default()
        };
        let pricing = Arc::new(TestPricingProvider {
            table: default_pricing_table(),
        });
        let tracker = CostTracker::new(
            config,
            "gpt-4o".to_string(),
            pricing,
            ModelCapabilities::default(),
        );
        assert!(tracker.is_enabled());
    }

    #[test]
    fn test_cost_tracker_enabled_with_per_turn_warning() {
        let config = CostBudgetConfig {
            per_turn_warning_usd: Some(0.5),
            ..Default::default()
        };
        let pricing = Arc::new(TestPricingProvider {
            table: default_pricing_table(),
        });
        let tracker = CostTracker::new(
            config,
            "gpt-4o".to_string(),
            pricing,
            ModelCapabilities::default(),
        );
        assert!(tracker.is_enabled());
    }

    #[test]
    fn test_estimate_tool_call_cost_known_model() {
        let config = CostBudgetConfig::default();
        let pricing = Arc::new(TestPricingProvider {
            table: default_pricing_table(),
        });
        let tracker = CostTracker::new(
            config,
            "gpt-4o".to_string(),
            pricing,
            ModelCapabilities::default(),
        );

        let cost = tracker.estimate_tool_call_cost(1000, 500).unwrap();
        assert_eq!(cost.input_tokens, 1000);
        assert_eq!(cost.output_tokens, 500);
        // gpt-4o: $2.50/1M input, $10.00/1M output
        assert!((cost.input_cost_usd - 0.0025).abs() < 0.0001);
        assert!((cost.output_cost_usd - 0.005).abs() < 0.0001);
    }

    #[test]
    fn test_estimate_tool_call_cost_unknown_model_fails_closed() {
        let config = CostBudgetConfig::default();
        let pricing = Arc::new(TestPricingProvider {
            table: default_pricing_table(),
        });
        let tracker = CostTracker::new(
            config,
            "unknown-model-xyz".to_string(),
            pricing,
            ModelCapabilities::default(),
        );

        let result = tracker.estimate_tool_call_cost(1000, 500);
        assert!(result.is_err());
        match result.unwrap_err() {
            CostBudgetError::UnknownPricing { model_id } => {
                assert_eq!(model_id, "unknown-model-xyz");
            }
            _ => panic!("expected UnknownPricing error"),
        }
    }

    #[test]
    fn test_check_session_cap_allows_under_cap() {
        let config = CostBudgetConfig {
            session_cap_usd: Some(10.0),
            ..Default::default()
        };
        let pricing = Arc::new(TestPricingProvider {
            table: default_pricing_table(),
        });
        let mut tracker = CostTracker::new(
            config,
            "gpt-4o".to_string(),
            pricing,
            ModelCapabilities::default(),
        );

        // Spend $1
        tracker.record_cost(&CostEstimate {
            input_tokens: 0,
            output_tokens: 0,
            input_cost_usd: 0.0,
            output_cost_usd: 1.0,
        });

        // Check $5 more - should be OK (total $6 < $10)
        assert!(tracker.check_session_cap(5.0).is_ok());
    }

    #[test]
    fn test_check_session_cap_rejects_over_cap() {
        let config = CostBudgetConfig {
            session_cap_usd: Some(10.0),
            ..Default::default()
        };
        let pricing = Arc::new(TestPricingProvider {
            table: default_pricing_table(),
        });
        let mut tracker = CostTracker::new(
            config,
            "gpt-4o".to_string(),
            pricing,
            ModelCapabilities::default(),
        );

        // Spend $8
        tracker.record_cost(&CostEstimate {
            input_tokens: 0,
            output_tokens: 0,
            input_cost_usd: 0.0,
            output_cost_usd: 8.0,
        });

        // Check $5 more - should reject (total $13 > $10)
        let result = tracker.check_session_cap(5.0);
        assert!(result.is_err());
        match result.unwrap_err() {
            CostBudgetError::CapExceeded {
                spent,
                cap,
                projected,
            } => {
                assert!((spent - 8.0).abs() < 0.001);
                assert!((cap - 10.0).abs() < 0.001);
                assert!((projected - 13.0).abs() < 0.001);
            }
            _ => panic!("expected CapExceeded error"),
        }
    }

    #[test]
    fn test_record_cost_accumulates() {
        let config = CostBudgetConfig::default();
        let pricing = Arc::new(TestPricingProvider {
            table: default_pricing_table(),
        });
        let mut tracker = CostTracker::new(
            config,
            "gpt-4o".to_string(),
            pricing,
            ModelCapabilities::default(),
        );

        assert_eq!(tracker.spent_usd(), 0.0);

        tracker.record_cost(&CostEstimate {
            input_tokens: 0,
            output_tokens: 0,
            input_cost_usd: 1.0,
            output_cost_usd: 2.0,
        });
        assert!((tracker.spent_usd() - 3.0).abs() < 0.001);

        tracker.record_cost(&CostEstimate {
            input_tokens: 0,
            output_tokens: 0,
            input_cost_usd: 0.5,
            output_cost_usd: 0.5,
        });
        assert!((tracker.spent_usd() - 4.0).abs() < 0.001);
    }

    #[test]
    fn test_summary_includes_remaining() {
        let config = CostBudgetConfig {
            session_cap_usd: Some(10.0),
            per_turn_warning_usd: Some(0.5),
            expensive_call_confirmation_usd: None,
        };
        let pricing = Arc::new(TestPricingProvider {
            table: default_pricing_table(),
        });
        let tracker = CostTracker::new(
            config,
            "gpt-4o".to_string(),
            pricing,
            ModelCapabilities::default(),
        );

        let summary = tracker.summary();
        assert_eq!(summary.spent_usd, 0.0);
        assert_eq!(summary.session_cap_usd, Some(10.0));
        assert_eq!(summary.per_turn_warning_usd, Some(0.5));
        assert_eq!(summary.expensive_call_confirmation_usd, None);
        assert_eq!(summary.model_id, "gpt-4o");
        assert_eq!(summary.remaining_usd, Some(10.0));
    }

    #[test]
    fn test_cost_tracker_enabled_with_expensive_call_confirmation() {
        let config = CostBudgetConfig {
            expensive_call_confirmation_usd: Some(0.5),
            ..Default::default()
        };
        let pricing = Arc::new(TestPricingProvider {
            table: default_pricing_table(),
        });
        let tracker = CostTracker::new(
            config,
            "gpt-4o".to_string(),
            pricing,
            ModelCapabilities::default(),
        );
        assert!(tracker.is_enabled());
    }

    #[test]
    fn test_requires_expensive_call_confirmation_below_threshold() {
        let config = CostBudgetConfig {
            expensive_call_confirmation_usd: Some(1.0),
            ..Default::default()
        };
        let pricing = Arc::new(TestPricingProvider {
            table: default_pricing_table(),
        });
        let tracker = CostTracker::new(
            config,
            "gpt-4o".to_string(),
            pricing,
            ModelCapabilities::default(),
        );
        // Cost well below threshold
        assert!(!tracker.requires_expensive_call_confirmation(0.1));
    }

    #[test]
    fn test_requires_expensive_call_confirmation_above_threshold() {
        let config = CostBudgetConfig {
            expensive_call_confirmation_usd: Some(0.1),
            ..Default::default()
        };
        let pricing = Arc::new(TestPricingProvider {
            table: default_pricing_table(),
        });
        let tracker = CostTracker::new(
            config,
            "gpt-4o".to_string(),
            pricing,
            ModelCapabilities::default(),
        );
        // Cost above threshold
        assert!(tracker.requires_expensive_call_confirmation(0.5));
    }

    #[test]
    fn test_requires_expensive_call_confirmation_disabled() {
        let config = CostBudgetConfig::default(); // No threshold set
        let pricing = Arc::new(TestPricingProvider {
            table: default_pricing_table(),
        });
        let tracker = CostTracker::new(
            config,
            "gpt-4o".to_string(),
            pricing,
            ModelCapabilities::default(),
        );
        // Disabled = never requires confirmation
        assert!(!tracker.requires_expensive_call_confirmation(1000.0));
    }

    #[test]
    fn test_summary_includes_expensive_call_confirmation() {
        let config = CostBudgetConfig {
            session_cap_usd: Some(10.0),
            per_turn_warning_usd: Some(0.5),
            expensive_call_confirmation_usd: Some(0.25),
        };
        let pricing = Arc::new(TestPricingProvider {
            table: default_pricing_table(),
        });
        let tracker = CostTracker::new(
            config,
            "gpt-4o".to_string(),
            pricing,
            ModelCapabilities::default(),
        );

        let summary = tracker.summary();
        assert_eq!(summary.expensive_call_confirmation_usd, Some(0.25));
    }
}
