//! Retry policy with bounded retries and exponential backoff.
//!
//! Provider errors (HTTP 429 rate-limited, 503 service unavailable) are
//! retried with exponential backoff. Permanent errors are never retried.

use crate::model::ModelError;
use rand::Rng;
use std::time::Duration;

/// Upper bound applied to provider `Retry-After` hints.
pub const MAX_RETRY_AFTER_SECS: u64 = 60;

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
    /// A provider `Retry-After` hint (non-zero `retry_after_sec`) is
    /// authoritative and used as-is, clamped to [`MAX_RETRY_AFTER_SECS`].
    /// Otherwise uses exponential backoff with equal jitter: half of
    /// `base_backoff_ms * 2^attempt` (capped at `max_backoff_ms`) plus a random
    /// amount up to the other half, to avoid synchronized retries.
    pub fn backoff_for_attempt(&self, attempt: usize, error: Option<&ModelError>) -> Duration {
        if let Some(
            ModelError::RateLimited { retry_after_sec }
            | ModelError::ServiceUnavailable { retry_after_sec },
        ) = error
            && *retry_after_sec > 0
        {
            return Duration::from_secs((*retry_after_sec).min(MAX_RETRY_AFTER_SECS));
        }

        let ceiling = self.exponential_ms(attempt);
        let half = ceiling / 2;
        let jitter = if half == 0 {
            0
        } else {
            rand::thread_rng().gen_range(0..=half)
        };
        Duration::from_millis(half + jitter)
    }

    /// Un-jittered exponential delay in ms for the given attempt.
    fn exponential_ms(&self, attempt: usize) -> u64 {
        let attempt = attempt.min(32) as u32;
        self.base_backoff_ms
            .saturating_mul(2u64.saturating_pow(attempt))
            .min(self.max_backoff_ms)
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
    fn test_stream_interrupted_is_retryable() {
        let policy = RetryPolicy::default();
        assert_eq!(
            policy.classify(&ModelError::StreamInterrupted("reset".into())),
            RetryDecision::Retry
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
    fn test_backoff_retry_after_is_clamped() {
        let policy = RetryPolicy::default();
        let err = ModelError::RateLimited {
            retry_after_sec: 3600,
        };
        assert_eq!(
            policy.backoff_for_attempt(0, Some(&err)),
            Duration::from_secs(MAX_RETRY_AFTER_SECS)
        );
    }

    #[test]
    fn test_backoff_zero_retry_after_falls_back_to_exponential() {
        let policy = RetryPolicy::default();
        let err = ModelError::RateLimited { retry_after_sec: 0 };
        for _ in 0..50 {
            let d = policy.backoff_for_attempt(0, Some(&err));
            assert!(d >= Duration::from_millis(250), "{d:?} should not be ~zero");
            assert!(d <= Duration::from_millis(500));
        }
    }

    #[test]
    fn test_backoff_exponential_growth_with_jitter() {
        let policy = RetryPolicy::default();
        for (attempt, ceiling) in [(0usize, 500u64), (1, 1000), (2, 2000)] {
            for _ in 0..50 {
                let d = policy.backoff_for_attempt(attempt, None);
                assert!(d >= Duration::from_millis(ceiling / 2), "{d:?}");
                assert!(d <= Duration::from_millis(ceiling), "{d:?}");
            }
        }
    }

    #[test]
    fn test_backoff_capped_at_max() {
        let policy = RetryPolicy {
            base_backoff_ms: 1000,
            max_backoff_ms: 1500,
            ..Default::default()
        };
        for _ in 0..50 {
            let d = policy.backoff_for_attempt(10, None);
            assert!(d <= Duration::from_millis(1500), "{d:?}");
            assert!(d >= Duration::from_millis(750), "{d:?}");
        }
        // Huge attempt counts must not overflow.
        assert!(policy.backoff_for_attempt(usize::MAX, None) <= Duration::from_millis(1500));
    }
}
