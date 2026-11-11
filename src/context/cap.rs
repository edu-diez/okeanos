//! Hard context cap enforcement.

use crate::context::tokens::{CostEstimate, TokenCounter};
use crate::events::Message;
use crate::model::ModelCapabilities;
use thiserror::Error;

/// Error raised when the context cap is exceeded.
#[derive(Debug, Error)]
#[error(
    "context cap exceeded: estimated {estimated} tokens > cap {cap} tokens (input: {input}, output: {output}, cost: ${cost:.4})"
)]
pub struct ContextCapError {
    pub estimated: usize,
    pub cap: usize,
    pub input: usize,
    pub output: usize,
    pub cost: f64,
}

/// Context cap configuration.
#[derive(Debug, Clone, Copy)]
pub struct ContextCap {
    /// Maximum tokens allowed in the conversation context.
    pub max_tokens: usize,
    /// Warning threshold as a fraction of max_tokens (e.g., 0.85 = warn at 85%).
    pub warning_threshold: f64,
    /// Token counter to use for estimation.
    pub token_counter: TokenCounter,
}

impl Default for ContextCap {
    fn default() -> Self {
        Self {
            max_tokens: 8192,
            warning_threshold: 0.85,
            token_counter: TokenCounter,
        }
    }
}

impl ContextCap {
    /// Create a new context cap with the given maximum tokens.
    pub fn new(max_tokens: usize) -> Self {
        Self {
            max_tokens,
            ..Default::default()
        }
    }

    /// Create a context cap from model capabilities.
    pub fn from_capabilities(capabilities: &ModelCapabilities) -> Self {
        Self::new(capabilities.max_context)
    }

    /// Check if adding the given tokens would exceed the cap.
    pub fn would_exceed(&self, current_tokens: usize, additional_tokens: usize) -> bool {
        current_tokens + additional_tokens > self.max_tokens
    }

    /// Check if we're at or above the warning threshold.
    pub fn at_warning_threshold(&self, current_tokens: usize) -> bool {
        current_tokens as f64 >= self.max_tokens as f64 * self.warning_threshold
    }

    /// Get remaining tokens before cap.
    pub fn remaining(&self, current_tokens: usize) -> usize {
        self.max_tokens.saturating_sub(current_tokens)
    }

    /// Get the warning threshold in absolute tokens.
    pub fn warning_token_count(&self) -> usize {
        (self.max_tokens as f64 * self.warning_threshold) as usize
    }

    /// Enforce the context cap, returning an error if exceeded.
    pub fn enforce(
        &self,
        current_tokens: usize,
        additional_tokens: usize,
    ) -> Result<(), ContextCapError> {
        let estimated = current_tokens + additional_tokens;
        if estimated > self.max_tokens {
            // Estimate cost for the error message
            let cost = 0.0; // Would need pricing info
            return Err(ContextCapError {
                estimated,
                cap: self.max_tokens,
                input: current_tokens,
                output: additional_tokens,
                cost,
            });
        }
        Ok(())
    }

    /// Enforce with cost estimation.
    pub fn enforce_with_cost(
        &self,
        current_tokens: usize,
        additional_tokens: usize,
        cost_estimate: &CostEstimate,
    ) -> Result<(), ContextCapError> {
        let estimated = current_tokens + additional_tokens;
        if estimated > self.max_tokens {
            return Err(ContextCapError {
                estimated,
                cap: self.max_tokens,
                input: current_tokens,
                output: additional_tokens,
                cost: cost_estimate.total_cost_usd(),
            });
        }
        Ok(())
    }

    /// Count tokens in a conversation.
    pub fn count_conversation(&self, messages: &[Message]) -> usize {
        messages
            .iter()
            .map(|m| {
                self.token_counter
                    .count_message(&m.role.to_string(), &m.content)
            })
            .sum()
    }

    /// Count tokens in a string.
    pub fn count_text(&self, text: &str) -> usize {
        self.token_counter.count(text)
    }
}

/// Warning levels for context usage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextWarning {
    /// Below warning threshold.
    Ok,
    /// At or above warning threshold but below cap.
    Warning,
    /// At or above cap.
    Critical,
}

impl ContextWarning {
    pub fn from_tokens(cap: &ContextCap, current_tokens: usize) -> Self {
        if current_tokens >= cap.max_tokens {
            ContextWarning::Critical
        } else if current_tokens >= cap.warning_token_count() {
            ContextWarning::Warning
        } else {
            ContextWarning::Ok
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ModelCapabilities;

    #[test]
    fn test_context_cap_default() {
        let cap = ContextCap::default();
        assert_eq!(cap.max_tokens, 8192);
        assert_eq!(cap.warning_threshold, 0.85);
    }

    #[test]
    fn test_context_cap_new() {
        let cap = ContextCap::new(4096);
        assert_eq!(cap.max_tokens, 4096);
    }

    #[test]
    fn test_context_cap_from_capabilities() {
        let capabilities = ModelCapabilities {
            max_context: 16384,
            ..Default::default()
        };
        let cap = ContextCap::from_capabilities(&capabilities);
        assert_eq!(cap.max_tokens, 16384);
    }

    #[test]
    fn test_would_exceed() {
        let cap = ContextCap::new(1000);
        assert!(!cap.would_exceed(500, 400)); // 900 <= 1000
        assert!(cap.would_exceed(500, 600)); // 1100 > 1000
    }

    #[test]
    fn test_at_warning_threshold() {
        let cap = ContextCap::new(1000); // warning at 850
        assert!(!cap.at_warning_threshold(800));
        assert!(cap.at_warning_threshold(850));
        assert!(cap.at_warning_threshold(900));
    }

    #[test]
    fn test_remaining() {
        let cap = ContextCap::new(1000);
        assert_eq!(cap.remaining(300), 700);
        assert_eq!(cap.remaining(1000), 0);
        assert_eq!(cap.remaining(1200), 0); // saturating
    }

    #[test]
    fn test_warning_token_count() {
        let cap = ContextCap::new(1000);
        assert_eq!(cap.warning_token_count(), 850);

        let cap = ContextCap::new(4096);
        assert_eq!(cap.warning_token_count(), 3481); // 4096 * 0.85 = 3481.6
    }

    #[test]
    fn test_enforce_ok() {
        let cap = ContextCap::new(1000);
        assert!(cap.enforce(500, 400).is_ok());
    }

    #[test]
    fn test_enforce_error() {
        let cap = ContextCap::new(1000);
        let err = cap.enforce(500, 600).unwrap_err();
        assert_eq!(err.estimated, 1100);
        assert_eq!(err.cap, 1000);
    }

    #[test]
    fn test_context_warning_levels() {
        let cap = ContextCap::new(1000);
        assert_eq!(ContextWarning::from_tokens(&cap, 500), ContextWarning::Ok);
        assert_eq!(
            ContextWarning::from_tokens(&cap, 850),
            ContextWarning::Warning
        );
        assert_eq!(
            ContextWarning::from_tokens(&cap, 950),
            ContextWarning::Warning
        );
        assert_eq!(
            ContextWarning::from_tokens(&cap, 1000),
            ContextWarning::Critical
        );
        assert_eq!(
            ContextWarning::from_tokens(&cap, 1100),
            ContextWarning::Critical
        );
    }
}
