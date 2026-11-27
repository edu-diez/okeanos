//! Shared HTTP client construction for provider calls.

use std::time::Duration;

const DEFAULT_CONNECT_SECS: u64 = 30;
/// Maximum silence between body chunks before the request is abandoned. SSE
/// streams normally emit keep-alives or tokens well within this window.
const DEFAULT_READ_SECS: u64 = 120;

fn env_secs(var: &str, default: u64) -> Duration {
    let secs = std::env::var(var)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|s| *s > 0)
        .unwrap_or(default);
    Duration::from_secs(secs)
}

/// Build a client with a connect timeout and a per-read idle timeout, so a
/// silent provider connection fails (as a retryable error) instead of hanging.
/// Overridable via `OKEANOS_HTTP_CONNECT_TIMEOUT_SECS` / `OKEANOS_HTTP_READ_TIMEOUT_SECS`.
pub fn provider_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(env_secs(
            "OKEANOS_HTTP_CONNECT_TIMEOUT_SECS",
            DEFAULT_CONNECT_SECS,
        ))
        .read_timeout(env_secs(
            "OKEANOS_HTTP_READ_TIMEOUT_SECS",
            DEFAULT_READ_SECS,
        ))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}
