//! Loop-level retry policy (issue #99).
//!
//! Provider-level retry (`RetryPolicy`, `crates/llm`) covers a *single* LLM
//! call. It is deliberately short (2 attempts by default) and knows nothing
//! about the surrounding multi-day loop, so once it gives up the error used
//! to bubble out of `AgentRuntime::run_loop` and kill the whole loop — a
//! few minutes of provider flapping ended a loop that was meant to run for
//! days.
//!
//! [`LoopRetryPolicy`] is the outer, much slower rung: a bounded number of
//! exponential-backoff retries *of the turn*, applied by `run_loop` only
//! when replaying the turn cannot duplicate work (no assistant output ever
//! landed — see `AgentRuntime::retry_is_safe`).
//!
//! The default budget is 4 retries backing off 5s → 60s. That rides out a
//! provider blip of roughly a minute to a few minutes without keeping an
//! operator stuck at a terminal for an unbounded time.

use std::time::Duration;

use crate::error::{is_context_window_exceeded, Error};

/// Bounded exponential backoff for failed loop turns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoopRetryPolicy {
    /// Retries after the first failure (so `max_retries + 1` attempts).
    pub max_retries: usize,
    /// Backoff after the first failure; doubles per attempt.
    pub initial_backoff: Duration,
    /// Ceiling for the doubling backoff.
    pub max_backoff: Duration,
}

impl Default for LoopRetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: 4,
            initial_backoff: Duration::from_secs(5),
            max_backoff: Duration::from_secs(60),
        }
    }
}

impl LoopRetryPolicy {
    pub fn new(max_retries: usize, initial_backoff: Duration, max_backoff: Duration) -> Self {
        Self {
            max_retries,
            initial_backoff,
            max_backoff,
        }
    }

    /// Backoff before the retry that follows failure `attempt` (0-indexed),
    /// or `None` when the retry budget is exhausted.
    pub fn backoff_for(&self, attempt: usize) -> Option<Duration> {
        if attempt >= self.max_retries {
            return None;
        }
        let backoff = self
            .initial_backoff
            .checked_mul(2u32.saturating_pow(attempt as u32))
            .unwrap_or(self.max_backoff);
        Some(backoff.min(self.max_backoff))
    }

    /// Whether a failed turn is worth replaying.
    ///
    /// Retryable: transport/rate-limit/timeout/IO failures, plus provider
    /// errors that are *not* the provider's final word. Everything else —
    /// cancellation, budget exhaustion, config, permissions, tool-argument
    /// errors, a too-long prompt — is deterministic: replaying it burns
    /// tokens and delays the honest failure.
    pub fn is_retryable(err: &Error) -> bool {
        match err {
            // `HTTP 4xx` is a request-shaped rejection (bad model, bad key,
            // bad payload); only 408/429 are worth another attempt. 5xx and
            // messages without a status stay retryable.
            Error::Llm { message, .. } => {
                !is_context_window_exceeded(err) && !is_permanent_http_message(message)
            }
            _ => err.is_transient(),
        }
    }
}

/// `true` when `message` carries an HTTP status the provider will keep
/// rejecting (4xx, minus the two transient ones).
fn is_permanent_http_message(message: &str) -> bool {
    let Some(rest) = message.split("HTTP ").nth(1) else {
        return false;
    };
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    let Ok(status) = digits.parse::<u16>() else {
        return false;
    };
    (400..500).contains(&status) && status != 408 && status != 429
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Error;

    fn llm(message: &str) -> Error {
        Error::Llm {
            provider: "mock".into(),
            message: message.into(),
        }
    }

    #[test]
    fn backoff_doubles_and_caps() {
        let p = LoopRetryPolicy::new(4, Duration::from_secs(5), Duration::from_secs(60));
        assert_eq!(p.backoff_for(0), Some(Duration::from_secs(5)));
        assert_eq!(p.backoff_for(1), Some(Duration::from_secs(10)));
        assert_eq!(p.backoff_for(2), Some(Duration::from_secs(20)));
        assert_eq!(p.backoff_for(3), Some(Duration::from_secs(40)));
    }

    #[test]
    fn backoff_is_none_once_budget_is_exhausted() {
        let p = LoopRetryPolicy::new(2, Duration::from_secs(5), Duration::from_secs(60));
        assert!(p.backoff_for(2).is_none());
        assert!(p.backoff_for(9).is_none());
    }

    #[test]
    fn backoff_never_exceeds_the_cap() {
        let p = LoopRetryPolicy::new(10, Duration::from_secs(30), Duration::from_secs(60));
        assert_eq!(p.backoff_for(1), Some(Duration::from_secs(60)));
        assert_eq!(p.backoff_for(4), Some(Duration::from_secs(60)));
    }

    #[test]
    fn default_budget_is_bounded_and_short() {
        let p = LoopRetryPolicy::default();
        assert_eq!(p.max_retries, 4);
        let total: Duration = (0..p.max_retries).filter_map(|a| p.backoff_for(a)).sum();
        assert!(
            total <= Duration::from_secs(120),
            "operator must not be parked for long; total {total:?}"
        );
    }

    #[test]
    fn transient_errors_are_retryable() {
        for err in [
            Error::RateLimited {
                provider: "mock".into(),
                retry_after_ms: 10,
            },
            Error::Timeout { duration_ms: 10 },
            llm("HTTP 503: upstream unavailable"),
            llm("connection reset by peer"),
            Error::Io(std::io::Error::other("boom")),
        ] {
            assert!(
                LoopRetryPolicy::is_retryable(&err),
                "{err} should be retried"
            );
        }
    }

    #[test]
    fn permanent_provider_rejections_are_not_retryable() {
        for err in [
            llm("HTTP 404: model not found"),
            llm("HTTP 401: invalid api key"),
            llm("HTTP 400: bad request"),
        ] {
            assert!(
                !LoopRetryPolicy::is_retryable(&err),
                "{err} must not be retried"
            );
        }
    }

    #[test]
    fn http_408_and_429_stay_retryable() {
        assert!(LoopRetryPolicy::is_retryable(&llm("HTTP 429: slow down")));
        assert!(LoopRetryPolicy::is_retryable(&llm("HTTP 408: timeout")));
    }

    #[test]
    fn context_window_exceeded_is_not_retryable() {
        assert!(!LoopRetryPolicy::is_retryable(&llm(
            "HTTP 400: maximum context length is 128000 tokens"
        )));
    }

    #[test]
    fn deterministic_failures_are_not_retryable() {
        for err in [
            Error::Cancelled,
            Error::WallClockExceeded { secs: 60 },
            Error::Config {
                message: "missing key".into(),
            },
            Error::BadToolArgs {
                name: "Read".into(),
                message: "missing path".into(),
            },
            Error::NotFound("session".into()),
        ] {
            assert!(
                !LoopRetryPolicy::is_retryable(&err),
                "{err} must not be retried"
            );
        }
    }
}
