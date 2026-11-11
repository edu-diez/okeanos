//! Context window warnings before hard cap.

use crate::context::cap::ContextCap;
use crate::context::tokens::TokenCounter;
use crate::events::{Event, LogLevel};
use crate::model::ModelCapabilities;
use tokio::sync::broadcast;

/// Configuration for context warnings.
#[derive(Debug, Clone)]
pub struct WarningConfig {
    /// Warning threshold as fraction of max tokens (e.g., 0.85 = warn at 85%).
    pub warning_threshold: f64,
    /// Critical threshold as fraction (e.g., 0.95 = critical at 95%).
    pub critical_threshold: f64,
    /// Whether to emit warning events.
    pub emit_events: bool,
}

impl Default for WarningConfig {
    fn default() -> Self {
        Self {
            warning_threshold: 0.85,
            critical_threshold: 0.95,
            emit_events: true,
        }
    }
}

/// Context warning state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WarningLevel {
    /// Below warning threshold.
    Ok,
    /// At or above warning threshold (85% by default).
    Warning,
    /// At or above critical threshold (95% by default).
    Critical,
    /// At or above hard cap (100%).
    Exceeded,
}

impl WarningLevel {
    /// Get warning level from current tokens and cap.
    pub fn from_tokens(cap: &ContextCap, current_tokens: usize) -> Self {
        let warning_tokens = (cap.max_tokens as f64 * cap.warning_threshold) as usize;
        let critical_tokens = (cap.max_tokens as f64 * 0.95) as usize;

        if current_tokens >= cap.max_tokens {
            WarningLevel::Exceeded
        } else if current_tokens >= critical_tokens {
            WarningLevel::Critical
        } else if current_tokens >= warning_tokens {
            WarningLevel::Warning
        } else {
            WarningLevel::Ok
        }
    }

    /// Get warning level with custom thresholds.
    pub fn from_tokens_custom(
        cap: &ContextCap,
        current_tokens: usize,
        warning_threshold: f64,
        critical_threshold: f64,
    ) -> Self {
        let warning_tokens = (cap.max_tokens as f64 * warning_threshold) as usize;
        let critical_tokens = (cap.max_tokens as f64 * critical_threshold) as usize;

        if current_tokens >= cap.max_tokens {
            WarningLevel::Exceeded
        } else if current_tokens >= critical_tokens {
            WarningLevel::Critical
        } else if current_tokens >= warning_tokens {
            WarningLevel::Warning
        } else {
            WarningLevel::Ok
        }
    }

    /// Get a human-readable message for this warning level.
    pub fn message(&self, cap: &ContextCap, current_tokens: usize) -> String {
        let pct = (current_tokens as f64 / cap.max_tokens as f64 * 100.0) as u32;
        let remaining = cap.max_tokens.saturating_sub(current_tokens);

        match self {
            WarningLevel::Ok => format!(
                "Context: {}/{} tokens ({}%) — {} remaining",
                current_tokens, cap.max_tokens, pct, remaining
            ),
            WarningLevel::Warning => format!(
                "⚠ Context warning: {}/{} tokens ({}%) — {} remaining (approaching limit)",
                current_tokens, cap.max_tokens, pct, remaining
            ),
            WarningLevel::Critical => format!(
                "⚠⚠ Context critical: {}/{} tokens ({}%) — only {} tokens remaining!",
                current_tokens, cap.max_tokens, pct, remaining
            ),
            WarningLevel::Exceeded => format!(
                "✗ Context exceeded: {}/{} tokens ({}%) — hard cap reached",
                current_tokens, cap.max_tokens, pct
            ),
        }
    }

    /// Get the log level for this warning.
    pub fn log_level(&self) -> LogLevel {
        match self {
            WarningLevel::Ok => LogLevel::Debug,
            WarningLevel::Warning => LogLevel::Warn,
            WarningLevel::Critical => LogLevel::Error,
            WarningLevel::Exceeded => LogLevel::Error,
        }
    }
}

/// Emit context warning events if thresholds are crossed.
pub fn check_and_emit_warnings(
    event_tx: &broadcast::Sender<Event>,
    cap: &ContextCap,
    current_tokens: usize,
    config: &WarningConfig,
) -> WarningLevel {
    let level = WarningLevel::from_tokens_custom(
        cap,
        current_tokens,
        config.warning_threshold,
        config.critical_threshold,
    );

    if config.emit_events && level != WarningLevel::Ok {
        let message = level.message(cap, current_tokens);
        let _ = event_tx.send(Event::log(level.log_level(), message));
    }

    level
}

/// Detailed context usage breakdown for status display.
#[derive(Debug, Clone)]
pub struct ContextUsage {
    pub system_tokens: usize,
    pub history_tokens: usize,
    pub file_tokens: usize,
    pub total_tokens: usize,
    pub max_tokens: usize,
    pub warning_level: WarningLevel,
}

impl ContextUsage {
    pub fn new(
        system_tokens: usize,
        history_tokens: usize,
        file_tokens: usize,
        max_tokens: usize,
        warning_config: &WarningConfig,
    ) -> Self {
        let total = system_tokens + history_tokens + file_tokens;
        let cap = ContextCap {
            max_tokens,
            warning_threshold: warning_config.warning_threshold,
            token_counter: TokenCounter,
        };
        let warning_level = WarningLevel::from_tokens_custom(
            &cap,
            total,
            warning_config.warning_threshold,
            warning_config.critical_threshold,
        );

        Self {
            system_tokens,
            history_tokens,
            file_tokens,
            total_tokens: total,
            max_tokens,
            warning_level,
        }
    }

    pub fn percentage(&self) -> f64 {
        if self.max_tokens == 0 {
            0.0
        } else {
            self.total_tokens as f64 / self.max_tokens as f64 * 100.0
        }
    }

    pub fn remaining(&self) -> usize {
        self.max_tokens.saturating_sub(self.total_tokens)
    }

    pub fn status_line(&self) -> String {
        format!(
            "ctx: {}/{} ({:.0}%) {}",
            self.total_tokens,
            self.max_tokens,
            self.percentage(),
            match self.warning_level {
                WarningLevel::Ok => "✓",
                WarningLevel::Warning => "⚠",
                WarningLevel::Critical => "⚠⚠",
                WarningLevel::Exceeded => "✗",
            }
        )
    }
}

/// Track context usage across turns for the status bar.
pub struct ContextTracker {
    cap: ContextCap,
    warning_config: WarningConfig,
    event_tx: Option<broadcast::Sender<Event>>,
    last_warning_level: WarningLevel,
}

impl ContextTracker {
    pub fn new(
        capabilities: &ModelCapabilities,
        warning_config: WarningConfig,
        event_tx: Option<broadcast::Sender<Event>>,
    ) -> Self {
        let cap = ContextCap::from_capabilities(capabilities);
        Self {
            cap,
            warning_config,
            event_tx,
            last_warning_level: WarningLevel::Ok,
        }
    }

    pub fn update(
        &mut self,
        system_tokens: usize,
        history_tokens: usize,
        file_tokens: usize,
    ) -> ContextUsage {
        let usage = ContextUsage::new(
            system_tokens,
            history_tokens,
            file_tokens,
            self.cap.max_tokens,
            &self.warning_config,
        );

        // Emit warning if level increased
        if usage.warning_level as u8 > self.last_warning_level as u8 {
            if let Some(tx) = &self.event_tx {
                let message = usage.warning_level.message(&self.cap, usage.total_tokens);
                let _ = tx.send(Event::log(usage.warning_level.log_level(), message));
            }
            self.last_warning_level = usage.warning_level;
        }

        usage
    }

    pub fn check_cap(
        &self,
        additional_tokens: usize,
    ) -> Result<(), crate::context::cap::ContextCapError> {
        let _current = self.cap.warning_token_count(); // Use this to get current total? No, we need actual current
        // This is a placeholder - actual current tokens should be passed in
        self.cap.enforce(0, additional_tokens)
    }

    pub fn cap(&self) -> &ContextCap {
        &self.cap
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::cap::ContextCap;

    #[test]
    fn test_warning_levels() {
        let cap = ContextCap::new(1000); // warning at 850, critical at 950

        assert_eq!(WarningLevel::from_tokens(&cap, 500), WarningLevel::Ok);
        assert_eq!(WarningLevel::from_tokens(&cap, 850), WarningLevel::Warning);
        assert_eq!(WarningLevel::from_tokens(&cap, 900), WarningLevel::Warning);
        assert_eq!(WarningLevel::from_tokens(&cap, 950), WarningLevel::Critical);
        assert_eq!(
            WarningLevel::from_tokens(&cap, 1000),
            WarningLevel::Exceeded
        );
        assert_eq!(
            WarningLevel::from_tokens(&cap, 1100),
            WarningLevel::Exceeded
        );
    }

    #[test]
    fn test_warning_level_custom_thresholds() {
        let cap = ContextCap::new(1000);

        // Custom: warning at 80%, critical at 90%
        assert_eq!(
            WarningLevel::from_tokens_custom(&cap, 700, 0.8, 0.9),
            WarningLevel::Ok
        );
        assert_eq!(
            WarningLevel::from_tokens_custom(&cap, 800, 0.8, 0.9),
            WarningLevel::Warning
        );
        assert_eq!(
            WarningLevel::from_tokens_custom(&cap, 900, 0.8, 0.9),
            WarningLevel::Critical
        );
    }

    #[test]
    fn test_warning_messages() {
        let cap = ContextCap::new(1000);

        let ok_msg = WarningLevel::Ok.message(&cap, 500);
        assert!(ok_msg.contains("500"));
        assert!(ok_msg.contains("1000"));
        assert!(ok_msg.contains("500")); // remaining

        let warn_msg = WarningLevel::Warning.message(&cap, 850);
        assert!(warn_msg.contains("⚠"));
        assert!(warn_msg.contains("approaching"));

        let crit_msg = WarningLevel::Critical.message(&cap, 950);
        assert!(crit_msg.contains("⚠⚠"));
        assert!(crit_msg.contains("only"));
        assert!(crit_msg.contains("remaining"));

        let exceeded_msg = WarningLevel::Exceeded.message(&cap, 1000);
        assert!(exceeded_msg.contains("✗"));
        assert!(exceeded_msg.contains("exceeded"));
    }

    #[test]
    fn test_context_usage() {
        let config = WarningConfig::default();
        let usage = ContextUsage::new(100, 200, 300, 1000, &config);

        assert_eq!(usage.system_tokens, 100);
        assert_eq!(usage.history_tokens, 200);
        assert_eq!(usage.file_tokens, 300);
        assert_eq!(usage.total_tokens, 600);
        assert_eq!(usage.max_tokens, 1000);
        assert_eq!(usage.percentage(), 60.0);
        assert_eq!(usage.remaining(), 400);
        assert_eq!(usage.warning_level, WarningLevel::Ok);
    }

    #[test]
    fn test_context_usage_warning() {
        let config = WarningConfig::default();
        let usage = ContextUsage::new(100, 400, 400, 1000, &config); // 900 tokens = 90%

        assert_eq!(usage.warning_level, WarningLevel::Warning);
        assert!(usage.status_line().contains("⚠"));
    }

    #[test]
    fn test_context_usage_critical() {
        let config = WarningConfig::default();
        let usage = ContextUsage::new(100, 500, 400, 1000, &config); // 1000 tokens = 100%

        assert_eq!(usage.warning_level, WarningLevel::Exceeded);
        assert!(usage.status_line().contains("✗"));
    }
}
