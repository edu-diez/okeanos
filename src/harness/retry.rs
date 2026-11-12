//! Retry policy with bounded retries and exponential backoff.
//!
//! Provider errors (HTTP 429 rate-limited, 503 service unavailable) are
//! retried with exponential backoff. Permanent errors are never retried.

use crate::model::ModelError;
use std::time::Duration;

/// Decision about whether to retry a model error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryDecision {
    /// The error is transient; retry with backoff.
    Retry,
    /// The error is permanent; do not retry.
    NoRetry,
}

/// Retry policy configuration for model provider calls.
#[derive(Debug, Clone)]
pub struct RetryPolicy {
    /// Maximum number of retry attempts (2–3 recommended per PRODUCT_SPEC FR-21).
    pub max_retries: usize,
    /// Base backoff delay in milliseconds. Actual delay is
    /// `base_backoff_ms * 2^attempt`, capped at `max_backoff_ms`.
    pub base_backoff_ms: u64,
    /// Maximum backoff delay in milliseconds.
    pub max_backoff_ms: u64,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: 3,
            base_backoff_ms: 500,
            max_backoff_ms: 8000,
        }
    }
}

impl RetryPolicy {
    /// Classify a model error as retryable or not.
    ///
    /// Retryable: `RateLimited` (429), `ServiceUnavailable` (503).
    /// Non-retryable: `AuthFailed`, `ModelNotFound`, `InvalidRequest`,
    /// `ContextLengthExceeded`, `MissingCapability`, `Streaming`, and plain
    /// `Provider` errors (unless they carry a known transient status).
    pub fn classify(&self, error: &ModelError) -> RetryDecision {
        if error.is_retryable() {
            return RetryDecision::Retry;
        }
        RetryDecision::NoRetry
    }

    /// Compute the backoff delay for a given attempt number (0-indexed).
    ///
    /// Uses exponential backoff: `base_backoff_ms * 2^attempt`, capped at
    /// `max_backoff_ms`. If the error carries a `retry_after_sec` value, that
    /// is used instead (it is authoritative).
    pub fn backoff_for_attempt(&self, attempt: usize, error: Option<&ModelError>) -> Duration {
        // Honor Retry-After / retry-after hint from the provider when present.
        if let Some(
            ModelError::RateLimited { retry_after_sec }
            | ModelError::ServiceUnavailable { retry_after_sec },
        ) = error
        {
            return Duration::from_secs(*retry_after_sec);
        }

        let backoff = self.base_backoff_ms * 2u64.saturating_pow(attempt as u32);
        let ms = backoff.min(self.max_backoff_ms);
        Duration::from_millis(ms)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ModelError;

    #[test]
    fn test_retry_policy_default() {
        let policy = RetryPolicy::default();
        assert_eq!(policy.max_retries, 3);
        assert_eq!(policy.base_backoff_ms, 500);
        assert_eq!(policy.max_backoff_ms, 8000);
    }

    #[test]
    fn test_classify_rate_limited_is_retryable() {
        let policy = RetryPolicy::default();
        assert_eq!(
            policy.classify(&ModelError::RateLimited { retry_after_sec: 5 }),
            RetryDecision::Retry
        );
    }

    #[test]
    fn test_classify_service_unavailable_is_retryable() {
        let policy = RetryPolicy::default();
        assert_eq!(
            policy.classify(&ModelError::ServiceUnavailable {
                retry_after_sec: 10
            }),
            RetryDecision::Retry
        );
    }

    #[test]
    fn test_classify_permanent_errors_are_not_retryable() {
        let policy = RetryPolicy::default();
        assert_eq!(
            policy.classify(&ModelError::AuthFailed),
            RetryDecision::NoRetry
        );
        assert_eq!(
            policy.classify(&ModelError::ModelNotFound("x".into())),
            RetryDecision::NoRetry
        );
        assert_eq!(
            policy.classify(&ModelError::InvalidRequest("bad".into())),
            RetryDecision::NoRetry
        );
        assert_eq!(
            policy.classify(&ModelError::ContextLengthExceeded("too long".into())),
            RetryDecision::NoRetry
        );
        assert_eq!(
            policy.classify(&ModelError::MissingCapability("tool_calling".into())),
            RetryDecision::NoRetry
        );
        assert_eq!(
            policy.classify(&ModelError::Streaming("broken".into())),
            RetryDecision::NoRetry
        );
        assert_eq!(
            policy.classify(&ModelError::Provider("some error".into())),
            RetryDecision::NoRetry
        );
    }

    #[test]
    fn test_is_retryable() {
        assert!(ModelError::RateLimited { retry_after_sec: 1 }.is_retryable());
        assert!(ModelError::ServiceUnavailable { retry_after_sec: 1 }.is_retryable());
        assert!(!ModelError::AuthFailed.is_retryable());
        assert!(!ModelError::ModelNotFound("x".into()).is_retryable());
        assert!(!ModelError::InvalidRequest("x".into()).is_retryable());
    }

    #[test]
    fn test_backoff_uses_retry_after() {
        let policy = RetryPolicy::default();
        let err = ModelError::RateLimited {
            retry_after_sec: 30,
        };
        let d = policy.backoff_for_attempt(0, Some(&err));
        assert_eq!(d, Duration::from_secs(30));
    }

    #[test]
    fn test_backoff_exponential_growth() {
        let policy = RetryPolicy::default();
        let d0 = policy.backoff_for_attempt(0, None);
        let d1 = policy.backoff_for_attempt(1, None);
        let d2 = policy.backoff_for_attempt(2, None);
        assert_eq!(d0, Duration::from_millis(500));
        assert_eq!(d1, Duration::from_millis(1000));
        assert_eq!(d2, Duration::from_millis(2000));
    }

    #[test]
    fn test_backoff_capped_at_max() {
        let policy = RetryPolicy {
            base_backoff_ms: 1000,
            max_backoff_ms: 1500,
            ..Default::default()
        };
        let d = policy.backoff_for_attempt(10, None);
        assert_eq!(d, Duration::from_millis(1500));
    }
}
