//! Token-bucket rate limiting and metrics middleware.

use axum::http::StatusCode;
use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::Mutex;

use super::Metrics;

/// Token-bucket rate limiter keyed by client identifier (API key or remote IP).
#[derive(Clone)]
pub struct RateLimiter {
    /// Tokens remaining per client key.
    buckets: Arc<Mutex<HashMap<String, TokenBucket>>>,
    /// Max tokens per bucket.
    capacity: u32,
    /// Tokens refilled per second.
    refill_rate: f64,
}

/// A single token bucket for one client.
struct TokenBucket {
    tokens: f64,
    last_refill: Instant,
}

impl RateLimiter {
    /// Create a new rate limiter with the given capacity and refill rate.
    ///
    /// - `capacity`: maximum number of tokens (burst size).
    /// - `refill_rate`: tokens added per second.
    ///
    /// External callers (tests, custom embedders) can construct a
    /// `RateLimiter` directly and inject it via
    /// [`build_router_with_auth_and_rate_limit`] when env-driven
    /// configuration is undesirable.
    pub fn new(capacity: u32, refill_rate: f64) -> Self {
        Self {
            buckets: Arc::new(Mutex::new(HashMap::new())),
            capacity,
            refill_rate,
        }
    }

    /// Check if a request from `key` is allowed.
    ///
    /// Returns `true` if the request is within the rate limit, `false` if it
    /// should be rejected (429).
    async fn check(&self, key: &str) -> bool {
        let mut buckets = self.buckets.lock().await;
        let now = Instant::now();

        let bucket = buckets.entry(key.to_string()).or_insert_with(|| {
            // New client gets a full bucket
            let tokens = self.capacity as f64;
            TokenBucket {
                tokens,
                last_refill: now,
            }
        });

        // Refill tokens based on elapsed time
        let elapsed = now.duration_since(bucket.last_refill).as_secs_f64();
        let refill = elapsed * self.refill_rate;
        bucket.tokens = (bucket.tokens + refill).min(self.capacity as f64);
        bucket.last_refill = now;

        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

/// Build a `RateLimiter` from environment variables.
///
/// - `RECURSIVE_RATE_LIMIT_RPM`: requests per minute (default: 60)
/// - `RECURSIVE_RATE_LIMIT_BURST`: burst capacity (default: 10)
pub(super) fn rate_limiter_from_env() -> RateLimiter {
    let rpm = std::env::var("RECURSIVE_RATE_LIMIT_RPM")
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .unwrap_or(60.0);
    let burst = std::env::var("RECURSIVE_RATE_LIMIT_BURST")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(10);
    // Convert RPM to per-second refill rate
    let refill_rate = rpm / 60.0;
    RateLimiter::new(burst, refill_rate)
}

/// Hash a string value using `DefaultHasher` to avoid storing raw secrets.
///
/// Returns a stable hex string for the given input. Not cryptographically
/// strong, but sufficient to prevent raw API key values from appearing in
/// memory dumps or logs.
fn hash_key(value: &str) -> String {
    let mut h = DefaultHasher::new();
    value.hash(&mut h);
    format!("{:016x}", h.finish())
}

/// Extract a client key from the request for rate limiting.
///
/// Uses the `X-API-Key` header if present, hashing the value so the raw
/// credential is never stored in the rate-limit bucket map (prevents leaking
/// keys via memory dumps). Falls back to the **leftmost** entry of
/// `X-Forwarded-For` when present (reverse-proxy deployments — the
/// previous form fell back to the **proxy** IP via `ConnectInfo`,
/// which is the same value for every client behind a load balancer;
/// NEW-HTTP-7 fix), then to the socket IP, then to `ip:unknown`.
///
/// The leftmost XFF entry is the original client per the de-facto
/// convention; **operators must configure their proxy to append
/// the client IP** (nginx `proxy_set_header X-Forwarded-For
/// $remote_addr;` does this by default). For a deployment
/// without a trusted proxy, the XFF header can be **forged** by
/// clients — in that case the proxy's own IP (the second XFF
/// entry) is more honest. We do not currently detect the
/// "no trusted proxy" case; documented as a follow-up.
pub(super) fn extract_client_key(req: &axum::extract::Request) -> String {
    if let Some(api_key) = req.headers().get("x-api-key") {
        if let Ok(key) = api_key.to_str() {
            return format!("apikey:{}", hash_key(key));
        }
    }
    // XFF first — leftmost is the original client per de-facto
    // convention. Trims whitespace; falls through if header is
    // present but empty.
    if let Some(xff) = req.headers().get("x-forwarded-for") {
        if let Ok(s) = xff.to_str() {
            if let Some(first) = s.split(',').next() {
                let first = first.trim();
                if !first.is_empty() {
                    return format!("xff:{first}");
                }
            }
        }
    }
    // Fall back to socket IP (the proxy's own IP behind a load
    // balancer, but still better than `ip:unknown` for direct
    // connections).
    req.extensions()
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
        .map(|info| format!("ip:{}", info.ip()))
        .unwrap_or_else(|| "ip:unknown".to_string())
}

/// Middleware that increments request counters.
pub(super) async fn metrics_middleware(
    axum::extract::State(metrics): axum::extract::State<Arc<Metrics>>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    metrics.requests_total.fetch_add(1, Ordering::Relaxed);
    metrics.requests_active.fetch_add(1, Ordering::Relaxed);
    let response = next.run(req).await;
    metrics.requests_active.fetch_sub(1, Ordering::Relaxed);
    response
}

/// Middleware that enforces rate limits on all API requests.
pub(super) async fn rate_limit_middleware(
    axum::extract::State(limiter): axum::extract::State<RateLimiter>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let key = extract_client_key(&req);
    if !limiter.check(&key).await {
        let mut resp = axum::response::Response::new(axum::body::Body::from("rate limit exceeded"));
        *resp.status_mut() = StatusCode::TOO_MANY_REQUESTS;
        return resp;
    }
    next.run(req).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// Helper: create a rate limiter with very small capacity for testing.
    fn test_limiter(capacity: u32, rpm: f64) -> RateLimiter {
        RateLimiter::new(capacity, rpm / 60.0)
    }

    #[tokio::test]
    async fn test_requests_within_limit_succeed() {
        let limiter = test_limiter(5, 60.0); // 5 burst, 60 RPM
        for _ in 0..5 {
            assert!(limiter.check("client-a").await, "request should be allowed");
        }
    }

    #[tokio::test]
    async fn test_requests_exceeding_limit_get_429() {
        let limiter = test_limiter(3, 60.0); // 3 burst, 60 RPM
        for _ in 0..3 {
            assert!(limiter.check("client-b").await, "request should be allowed");
        }
        // Fourth request should be denied
        assert!(
            !limiter.check("client-b").await,
            "request should be rate limited"
        );
    }

    #[tokio::test]
    async fn test_tokens_refill_after_waiting() {
        let limiter = test_limiter(2, 60.0); // 2 burst, 60 RPM = 1 per second
                                             // Exhaust the bucket
        assert!(limiter.check("client-c").await);
        assert!(limiter.check("client-c").await);
        assert!(!limiter.check("client-c").await, "should be denied");

        // Wait for refill (1 token per second, wait 1.1s to be safe)
        tokio::time::sleep(Duration::from_millis(1100)).await;

        // Should have at least 1 token now
        assert!(
            limiter.check("client-c").await,
            "should be allowed after refill"
        );
    }

    #[tokio::test]
    async fn test_different_clients_have_independent_buckets() {
        let limiter = test_limiter(2, 60.0); // 2 burst

        // Exhaust client-d
        assert!(limiter.check("client-d").await);
        assert!(limiter.check("client-d").await);
        assert!(
            !limiter.check("client-d").await,
            "client-d should be denied"
        );

        // client-e should still have a full bucket
        assert!(
            limiter.check("client-e").await,
            "client-e should be allowed"
        );
        assert!(
            limiter.check("client-e").await,
            "client-e should be allowed"
        );
    }

    #[tokio::test]
    async fn test_extract_client_key_with_api_key() {
        let req = axum::http::Request::builder()
            .header("x-api-key", "test-key-123")
            .body(axum::body::Body::empty())
            .unwrap();
        let key = extract_client_key(&req);
        // Key is prefixed with "apikey:" and the raw credential is not stored.
        assert!(
            key.starts_with("apikey:"),
            "expected apikey: prefix, got: {key}"
        );
        assert!(
            !key.contains("test-key-123"),
            "raw API key must not appear in bucket key"
        );
        // Stable: same input produces the same bucket key.
        let req2 = axum::http::Request::builder()
            .header("x-api-key", "test-key-123")
            .body(axum::body::Body::empty())
            .unwrap();
        assert_eq!(
            key,
            extract_client_key(&req2),
            "bucket key must be deterministic"
        );
        // Different keys produce different bucket keys.
        let req3 = axum::http::Request::builder()
            .header("x-api-key", "other-key-456")
            .body(axum::body::Body::empty())
            .unwrap();
        assert_ne!(
            key,
            extract_client_key(&req3),
            "different keys must have different buckets"
        );
    }

    #[tokio::test]
    async fn test_extract_client_key_without_api_key() {
        let req = axum::http::Request::builder()
            .body(axum::body::Body::empty())
            .unwrap();
        let key = extract_client_key(&req);
        // No ConnectInfo extension, so falls back to "ip:unknown"
        assert_eq!(key, "ip:unknown");
    }

    #[tokio::test]
    async fn test_rate_limiter_from_env_defaults() {
        // Unset the env vars to test defaults
        std::env::remove_var("RECURSIVE_RATE_LIMIT_RPM");
        std::env::remove_var("RECURSIVE_RATE_LIMIT_BURST");
        let limiter = rate_limiter_from_env();
        // Default: 60 RPM, 10 burst
        for _ in 0..10 {
            assert!(limiter.check("default-client").await);
        }
        assert!(!limiter.check("default-client").await, "burst exceeded");
    }
}

#[cfg(test)]
mod goal_h3_xff {
    use super::*;
    use axum::extract::Request;

    fn build_request_with_xff(xff: Option<&str>) -> Request {
        let mut req = Request::builder()
            .uri("/")
            .body(axum::body::Body::empty())
            .unwrap();
        if let Some(xff) = xff {
            req.headers_mut()
                .insert("x-forwarded-for", xff.parse().expect("valid header value"));
        }
        req
    }

    #[test]
    fn extract_client_key_uses_xff_when_no_apikey() {
        // NEW-HTTP-7: reverse-proxy deployments. The
        // leftmost XFF entry is the original client per the
        // de-facto convention. The previous form fell back
        // to the proxy's socket IP, which is the same value
        // for every client behind a load balancer — every
        // request shared one rate-limit bucket, so the
        // rate-limit effectively applied to the LB, not the
        // client. With XFF support each client gets its own
        // bucket.
        let req = build_request_with_xff(Some("203.0.113.42, 10.0.0.1"));
        let key = extract_client_key(&req);
        assert_eq!(key, "xff:203.0.113.42");
    }

    #[test]
    fn extract_client_key_trims_whitespace_in_xff() {
        let req = build_request_with_xff(Some("  203.0.113.42  ,  10.0.0.1  "));
        let key = extract_client_key(&req);
        assert_eq!(key, "xff:203.0.113.42");
    }

    #[test]
    fn extract_client_key_falls_back_when_xff_empty() {
        // An XFF header that is present but empty is treated as
        // absent — fall through to the socket-IP branch.
        let req = build_request_with_xff(Some(""));
        let key = extract_client_key(&req);
        assert!(
            key.starts_with("ip:"),
            "empty XFF should fall through to socket IP, got {key}"
        );
    }
}
