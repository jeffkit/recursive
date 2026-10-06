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
    /// Number of trusted reverse proxies in front of this server (#107).
    /// Each trusted proxy appends exactly one `X-Forwarded-For` entry (the
    /// peer address it saw), so with `n` trusted proxies the client address
    /// is the `n`-th entry counting from the **right**; everything further
    /// left was sent by the client and is discarded. `0` (direct exposure,
    /// the default) means the header is never trusted and the socket IP is
    /// used — a client cannot mint fresh buckets by rotating a header. A
    /// chain with fewer than `n` entries fails closed (socket-IP fallback),
    /// so a hop count larger than the real one can never select a
    /// client-supplied entry.
    trusted_proxies: usize,
    /// Burst guard: target ceiling on stored buckets (#107). When the map is
    /// at `max_buckets` the least-recently-refilled *idle* bucket is evicted
    /// first — idle meaning refilled back to capacity per the clock
    /// ([`is_idle`]), since a drained bucket's stored counter never reaches
    /// capacity; eviction prefers header-derived (`xff:`) keys over socket
    /// (`ip:`) and credential (`apikey:`) keys, so a header-rotating flood
    /// cannot crowd out real clients. If every bucket has been used within
    /// the refill window the insert proceeds WITHOUT eviction, so the map can
    /// exceed the cap by the number of not-yet-idle buckets — those are
    /// reclaimed once they refill, by this eviction and by the periodic
    /// `prune()` sweep (same projection). Unbounded growth would otherwise
    /// let one fake-IP flood exhaust memory.
    max_buckets: usize,
}

/// A single token bucket for one client.
struct TokenBucket {
    tokens: f64,
    last_refill: Instant,
}

/// Default hard cap on tracked client buckets (`RECURSIVE_RATE_LIMIT_MAX_BUCKETS`).
const DEFAULT_MAX_BUCKETS: usize = 10_000;

/// Refill rate (tokens/second) used by the prune/eviction tests. With burst 2
/// a drained bucket stores `capacity - 1` and is idle 200 ms later, so the
/// tests reach the idle state through `check()` + a sleep rather than by
/// writing `tokens` (which is exactly how the counter-only predicate used to
/// be kept green).
#[cfg(test)]
const IDLE_REFILL_RATE: f64 = 5.0;
/// Sleep that reliably exceeds the [`IDLE_REFILL_RATE`] refill window.
#[cfg(test)]
const IDLE_WAIT: std::time::Duration = std::time::Duration::from_millis(250);

impl RateLimiter {
    /// Create a new rate limiter with the given capacity and refill rate.
    ///
    /// - `capacity`: maximum number of tokens (burst size).
    /// - `refill_rate`: tokens added per second.
    ///
    /// External callers (tests, custom embedders) can construct a
    /// `RateLimiter` directly and inject it via
    /// [`build_router_with_auth_and_rate_limit`] when env-driven
    /// configuration is undesirable. The limiter starts with safe defaults:
    /// no trusted proxies (XFF never trusted) and a bounded bucket map.
    /// Use [`RateLimiter::with_trusted_proxies`] /
    /// [`RateLimiter::with_max_buckets`] to adjust.
    pub fn new(capacity: u32, refill_rate: f64) -> Self {
        Self {
            buckets: Arc::new(Mutex::new(HashMap::new())),
            capacity,
            refill_rate,
            trusted_proxies: 0,
            max_buckets: DEFAULT_MAX_BUCKETS,
        }
    }

    /// Declare how many reverse-proxy hops in front of this server are
    /// trusted to have appended their entries to `X-Forwarded-For` (#107).
    /// See the [`RateLimiter`] `trusted_proxies` field docs.
    pub fn with_trusted_proxies(mut self, n: usize) -> Self {
        self.trusted_proxies = n;
        self
    }

    /// Override the hard cap on tracked client buckets (#107).
    pub fn with_max_buckets(mut self, n: usize) -> Self {
        self.max_buckets = n.max(1);
        self
    }

    /// Check if a request from `key` is allowed.
    ///
    /// Returns `true` if the request is within the rate limit, `false` if it
    /// should be rejected (429).
    async fn check(&self, key: &str) -> bool {
        let mut buckets = self.buckets.lock().await;
        let now = Instant::now();

        // Bound the map before inserting a NEW key (#107). Eviction picks a
        // victim deterministically: idle buckets first — a bucket is idle
        // once it has refilled back to capacity, which is derived from the
        // clock by `projected_tokens`, NOT read off `tokens` (a drained
        // bucket always stores `capacity - 1`) — safe to drop since a
        // re-arriving client would get a fresh full bucket anyway —
        // preferring header-derived (`xff:`) keys over socket (`ip:`) and
        // authenticated (`apikey:`) ones so a header-rotating flood cannot
        // crowd out real clients. When every bucket has been used within the
        // refill window we reject the eviction (the caller still gets its
        // token decision) — the alternative (evicting an active
        // attacker-controlled bucket) would hand the attacker a fresh full
        // bucket on every request.
        if !buckets.contains_key(key) && buckets.len() >= self.max_buckets {
            if let Some(victim) = pick_eviction_victim(&buckets, self.capacity, self.refill_rate) {
                buckets.remove(&victim);
            }
        }

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

    /// Remove idle token buckets (refilled back to capacity).
    ///
    /// Idleness is decided by [`is_idle`], i.e. projected from the clock with
    /// the same refill formula [`check`](Self::check) applies — a bucket
    /// drained an hour ago is idle even though its stored counter still reads
    /// `capacity - 1`. Safe to drop: a re-arriving client gets a fresh full
    /// bucket, which is the same as the stored state.
    pub(super) async fn prune(&self) {
        let mut buckets = self.buckets.lock().await;
        buckets.retain(|_, b| !is_idle(b, self.capacity, self.refill_rate));
    }

    /// Return the number of client buckets currently stored.
    #[cfg(test)]
    pub(super) async fn bucket_count(&self) -> usize {
        self.buckets.lock().await.len()
    }
}

/// Build a `RateLimiter` from environment variables.
///
/// - `RECURSIVE_RATE_LIMIT_RPM`: requests per minute (default: 60)
/// - `RECURSIVE_RATE_LIMIT_BURST`: burst capacity (default: 10)
/// - `RECURSIVE_RATE_LIMIT_TRUSTED_PROXIES`: trusted reverse-proxy hops whose
///   `X-Forwarded-For` entries may be used as the client address (default: 0
///   = never trust XFF, use the socket IP)
/// - `RECURSIVE_RATE_LIMIT_MAX_BUCKETS`: cap on tracked client buckets before
///   eviction kicks in (default: 10 000)
pub fn rate_limiter_from_env() -> RateLimiter {
    let rpm = std::env::var("RECURSIVE_RATE_LIMIT_RPM")
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .unwrap_or(60.0);
    let burst = std::env::var("RECURSIVE_RATE_LIMIT_BURST")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(10);
    if rpm <= 0.0 {
        tracing::warn!("RECURSIVE_RATE_LIMIT_RPM=0: token bucket never refills, clients will be permanently blocked after burst");
    }
    if burst == 0 {
        tracing::warn!("RECURSIVE_RATE_LIMIT_BURST=0: all requests will be rejected immediately");
    }
    // Convert RPM to per-second refill rate
    let refill_rate = rpm / 60.0;
    let trusted_proxies = std::env::var("RECURSIVE_RATE_LIMIT_TRUSTED_PROXIES")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok());
    let max_buckets = std::env::var("RECURSIVE_RATE_LIMIT_MAX_BUCKETS")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok());
    let mut limiter = RateLimiter::new(burst, refill_rate);
    if let Some(n) = trusted_proxies {
        limiter = limiter.with_trusted_proxies(n);
    }
    if let Some(n) = max_buckets {
        limiter = limiter.with_max_buckets(n);
    }
    if limiter.trusted_proxies == 0 {
        // #107: without trusted proxies the XFF header is client-controlled.
        tracing::info!(
            "RECURSIVE_RATE_LIMIT_TRUSTED_PROXIES unset/0: X-Forwarded-For is \
             NOT trusted for rate limiting (direct-exposure posture). Set it to \
             your proxy hop count if deployed behind a reverse proxy."
        );
    }
    limiter
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
/// Identity priority (#107):
///
/// 1. `apikey:<hash>` — a non-empty `X-API-Key` header. This middleware runs
///    *before* authentication, so the value is not validated here; it only
///    partitions buckets. Keying on the credential is still the right
///    identity for the clients that do present one (the shared-bucket
///    complaint is a *tenanting* problem — per-subject buckets need
///    per-subject credentials, i.e. JWT `sub` — not a key-extraction one).
/// 2. `xff:<client>` — only when [`RateLimiter`] was configured with at
///    least one trusted proxy hop (`RECURSIVE_RATE_LIMIT_TRUSTED_PROXIES`,
///    `extract_client_key_with_config`). The client address is then the
///    `trusted_proxies`-th entry counting from the **right** of the
///    `X-Forwarded-For` chain: each trusted proxy appends the address it
///    saw, so the entries further left are client-supplied and discarded
///    rather than trusted. **The leftmost XFF entry is never used — it is
///    the one header field a client fully controls** (the previous
///    behaviour let a direct-connected client mint a fresh full bucket per
///    request by rotating the header).
/// 3. `ip:<socket>` — the socket address (direct deployments). All
///    unauthenticated clients behind one load balancer share this bucket;
///    that is the honest (conservative) reading — proxy deployments should
///    configure trusted-proxies instead.
///
/// The API-key value is hashed before use so raw credentials never sit in
/// the bucket map (memory-dump hygiene, M5).
///
/// The zero-config wrapper (XFF never trusted) is the test-facing entry
/// point; the middleware goes through
/// [`extract_client_key_with_config`] with the limiter's hop count.
#[cfg(test)]
pub(super) fn extract_client_key(req: &axum::extract::Request) -> String {
    extract_client_key_with_config(req, 0)
}

/// [`extract_client_key`] with an explicit trusted-proxy-hop count (#107).
///
/// `trusted_proxies = n` means: the request traversed `n` proxies we trust
/// to have appended the peer address they saw to XFF. The client address is
/// therefore the `n`-th XFF entry counting from the right, and every entry
/// further left is untrusted (client-supplied). If XFF is missing or has
/// fewer than `n` entries, the chain is not trustworthy — fall through to
/// the socket IP rather than accept a possibly-forged entry.
pub(super) fn extract_client_key_with_config(
    req: &axum::extract::Request,
    trusted_proxies: usize,
) -> String {
    if let Some(api_key) = req.headers().get("x-api-key") {
        if let Ok(key) = api_key.to_str() {
            if !key.is_empty() {
                return format!("apikey:{}", hash_key(key));
            }
        }
    }
    if trusted_proxies > 0 {
        if let Some(client) = trusted_xff_client(req, trusted_proxies) {
            return format!("xff:{client}");
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

/// Resolve the client address from `X-Forwarded-For` given a trusted-proxy
/// hop count (#107).
///
/// XFF semantics (de-facto standard / RFC 7239 `for=`): every proxy appends
/// the address of the peer it received the request from. A request that
/// traversed `n` trusted proxies therefore carries the `n` entries they
/// appended on the **right**, preceded by whatever untrusted entries the
/// client itself sent. The client address is the `n`-th entry from the right
/// (`entries[len - n]`); everything further left is discarded. Example with
/// `n = 1`: `"client"` (honest proxy) and `"forged, client"` (client
/// prepended a fake hop) both resolve to `client`.
///
/// Returns `None` (→ socket-IP fallback) when the header is absent, empty, or
/// carries fewer than `n` entries — a short chain means a trusted proxy did
/// not append, so no entry can be attributed to it.
fn trusted_xff_client(req: &axum::extract::Request, trusted_proxies: usize) -> Option<String> {
    // A request may carry several `X-Forwarded-For` header *fields*: a proxy
    // that emits its own field instead of appending to the client's (or a
    // middlebox) yields a second field, and `Headers::get` would then read
    // only the client-controlled first one — putting the forged entry back at
    // `len - n`. Flatten every field left-to-right (a single `"a, b"` field
    // and two separate fields produce the same ordered entry list) before
    // indexing, so the trusted-hop count always lands on a proxy-appended
    // entry. Any non-UTF-8 field fails closed to the socket IP.
    let mut entries: Vec<&str> = Vec::new();
    for value in req.headers().get_all("x-forwarded-for") {
        let value = value.to_str().ok()?;
        entries.extend(value.split(',').map(str::trim));
    }
    // Each trusted hop appended exactly one entry, so the client is entry
    // `len - n`. `checked_sub` is `None` when the chain is shorter than `n`
    // (a trusted hop is missing → fail closed); `get` is `None` for `n == 0`
    // (callers never ask for that, the socket IP is used instead).
    let client = *entries.get(entries.len().checked_sub(trusted_proxies)?)?;
    if client.is_empty() {
        return None;
    }
    Some(client.to_string())
}

/// Tokens a bucket would hold now if it were refilled, capped at `capacity`.
///
/// This is the same refill [`RateLimiter::check`] applies. The stored
/// counter alone cannot answer "is this bucket idle?": `check` always
/// decrements after clamping, so a bucket that has been idle for an hour
/// still stores `capacity - 1`. Idleness has to be projected from the clock
/// (`tokens + elapsed × refill_rate`) instead.
fn projected_tokens(bucket: &TokenBucket, capacity: u32, refill_rate: f64) -> f64 {
    (bucket.tokens + bucket.last_refill.elapsed().as_secs_f64() * refill_rate).min(capacity as f64)
}

/// Whether a bucket has refilled back to capacity — dropping it and
/// re-creating it on the next request are indistinguishable.
fn is_idle(bucket: &TokenBucket, capacity: u32, refill_rate: f64) -> bool {
    projected_tokens(bucket, capacity, refill_rate) >= capacity as f64 - f64::EPSILON
}

/// Pick the bucket to evict when the map is at its hard cap (#107).
///
/// Preference order:
/// 1. an idle bucket (refilled back to capacity per [`is_idle`] — dropping it
///    is indistinguishable from the prune sweep), taken from the class that is
///    cheapest for an attacker to mint first: header-derived `xff:` keys,
///    then socket-derived `ip:` keys, then credential-derived `apikey:`
///    keys. Within a class the least-recently-refilled bucket goes first.
/// 2. `None` — every bucket is partially drained (actively rate-limited
///    clients); evicting one would grant a fresh full bucket. The insert
///    proceeds WITHOUT eviction instead, so the map can exceed the cap by the
///    number of not-yet-idle buckets — roughly (new keys per second) ×
///    (seconds for a drained bucket to refill to idle), since only
///    refilled-to-capacity buckets are evictable. Keys untouched for that
///    refill window are reclaimed by this eviction and by the periodic
///    `prune()` sweep in the server reaper (same projection); this cap only
///    keeps a single burst from growing the map without limit.
///
/// Cost: an `O(max_buckets)` scan under the limiter mutex, and it runs only
/// when a *new* key arrives at cap. A client that mints a fresh key per
/// request (e.g. rotating an unvalidated pre-auth `x-api-key`) can therefore
/// force a full scan per request while the map is at cap — acceptable at the
/// default cap of 10 000, but not free.
fn pick_eviction_victim(
    buckets: &HashMap<String, TokenBucket>,
    capacity: u32,
    refill_rate: f64,
) -> Option<String> {
    // Eviction priority: HIGHER rank is evicted first. `xff:` keys are the
    // header-derived class — cheapest to mint, since the origin may also be
    // reachable around the proxy; `ip:` keys come from the socket and cannot
    // be chosen by the client; `apikey:` keys are what real clients hold.
    let class_rank = |key: &str| -> u8 {
        if key.starts_with("apikey:") {
            0
        } else if key.starts_with("xff:") {
            2
        } else {
            1
        }
    };
    let mut best: Option<(&String, &TokenBucket)> = None;
    for (k, b) in buckets.iter() {
        if !is_idle(b, capacity, refill_rate) {
            continue;
        }
        // Victim preference: HIGHER class rank first (header-derived before
        // socket before authenticated), then the oldest last_refill within
        // the class.
        let better = match best {
            None => true,
            Some((bk, bb)) => {
                let (r, br) = (class_rank(k), class_rank(bk));
                r > br || (r == br && b.last_refill < bb.last_refill)
            }
        };
        if better {
            best = Some((k, b));
        }
    }
    best.map(|(k, _)| k.clone())
}

/// Middleware that counts every request by matched route and response status
/// (issue #113) and tracks the in-flight gauge.
///
/// The route label comes from [`axum::extract::MatchedPath`], which the router
/// inserts before the per-route layers run — so it is the **route template**
/// (`/sessions/{id}/messages`), not the concrete path. That is what keeps the
/// `route` dimension bounded; labelling by `req.uri().path()` would mint one
/// series per session id. A request no route matched (404) is labelled
/// `unmatched`.
///
/// The status is read *after* the inner service runs, so a 5xx — including the
/// 503 an admission-saturated pool returns — is recorded against the route
/// that produced it.
pub(super) async fn metrics_middleware(
    axum::extract::State(metrics): axum::extract::State<Arc<Metrics>>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let route = req
        .extensions()
        .get::<axum::extract::MatchedPath>()
        .map(|p| p.as_str().to_string())
        .unwrap_or_else(|| "unmatched".to_string());
    metrics.requests_active.fetch_add(1, Ordering::Relaxed);
    let response = next.run(req).await;
    metrics.requests_active.fetch_sub(1, Ordering::Relaxed);
    metrics
        .requests_by_route
        .inc(&[&route, &response.status().as_u16().to_string()]);
    response
}

/// Middleware that enforces rate limits on all API requests.
pub(super) async fn rate_limit_middleware(
    axum::extract::State((limiter, metrics)): axum::extract::State<(RateLimiter, Arc<Metrics>)>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let key = extract_client_key_with_config(&req, limiter.trusted_proxies);
    if !limiter.check(&key).await {
        metrics.rate_limits_rejected.fetch_add(1, Ordering::Relaxed);
        let mut resp = axum::response::Response::new(axum::body::Body::from("rate limit exceeded"));
        *resp.status_mut() = StatusCode::TOO_MANY_REQUESTS;
        return resp;
    }
    next.run(req).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::routing::get;
    use std::time::Duration;
    use tower::ServiceExt;

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

    /// #107: the rate-limit env knobs parse with the same leniency as
    /// RPM/BURST, and garbage keeps the safe defaults. ONE test — env vars
    /// are process globals and parallel assertions would race
    /// (`.dev/AGENTS.md`: "Env-var tests must be ONE test").
    #[tokio::test]
    async fn rate_limiter_from_env_defaults_and_overrides() {
        // Unset everything → defaults (60 RPM, 10 burst, XFF untrusted, 10k cap).
        std::env::remove_var("RECURSIVE_RATE_LIMIT_RPM");
        std::env::remove_var("RECURSIVE_RATE_LIMIT_BURST");
        std::env::remove_var("RECURSIVE_RATE_LIMIT_TRUSTED_PROXIES");
        std::env::remove_var("RECURSIVE_RATE_LIMIT_MAX_BUCKETS");
        let limiter = rate_limiter_from_env();
        assert_eq!(limiter.trusted_proxies, 0);
        assert_eq!(limiter.max_buckets, DEFAULT_MAX_BUCKETS);
        for _ in 0..10 {
            assert!(limiter.check("default-client").await);
        }
        assert!(!limiter.check("default-client").await, "burst exceeded");

        // Explicit values are honoured.
        std::env::set_var("RECURSIVE_RATE_LIMIT_TRUSTED_PROXIES", "2");
        std::env::set_var("RECURSIVE_RATE_LIMIT_MAX_BUCKETS", "7");
        let limiter = rate_limiter_from_env();
        assert_eq!(limiter.trusted_proxies, 2);
        assert_eq!(limiter.max_buckets, 7);

        // Garbage → defaults (trusted 0 = XFF untrusted; cap 10k).
        std::env::set_var("RECURSIVE_RATE_LIMIT_TRUSTED_PROXIES", "nope");
        std::env::set_var("RECURSIVE_RATE_LIMIT_MAX_BUCKETS", "");
        let limiter = rate_limiter_from_env();
        assert_eq!(limiter.trusted_proxies, 0);
        assert_eq!(limiter.max_buckets, DEFAULT_MAX_BUCKETS);

        // max_buckets floor: 0 is clamped to 1 (a limiter that can never
        // store anything is never useful, but must not panic either).
        std::env::set_var("RECURSIVE_RATE_LIMIT_MAX_BUCKETS", "0");
        let limiter = rate_limiter_from_env();
        assert_eq!(limiter.max_buckets, 1);

        std::env::remove_var("RECURSIVE_RATE_LIMIT_TRUSTED_PROXIES");
        std::env::remove_var("RECURSIVE_RATE_LIMIT_MAX_BUCKETS");
    }

    /// #107: pin the middleware↔limiter wiring — the middleware must derive
    /// the bucket key from the limiter's configured hop count, never a
    /// hardcoded default. Capacity 1 / refill 0 makes bucket *sharing*
    /// observable: at `trusted = 1` distinct XFF values are distinct buckets
    /// (both allowed) while a repeat shares one (429); at `trusted = 0` the
    /// XFF is ignored and every request shares the `ip:unknown` bucket (the
    /// second distinct-XFF request is already 429).
    #[tokio::test]
    async fn middleware_derives_key_from_limiter_trust_config() {
        use axum::routing::get;
        let app = |trusted: usize| {
            let limiter = RateLimiter::new(1, 0.0).with_trusted_proxies(trusted);
            axum::Router::new()
                .route("/", get(|| async { "ok" }))
                .layer(axum::middleware::from_fn_with_state(
                    (limiter, Arc::new(Metrics::default())),
                    rate_limit_middleware,
                ))
        };
        let hit = |app: axum::Router, xff: &'static str| async move {
            app.oneshot(
                axum::http::Request::builder()
                    .uri("/")
                    .header("x-forwarded-for", xff)
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
            .status()
        };

        // trusted = 1: the header keys the bucket.
        let trusted = app(1);
        assert_eq!(hit(trusted.clone(), "9.9.9.1").await, StatusCode::OK);
        assert_eq!(hit(trusted.clone(), "9.9.9.2").await, StatusCode::OK);
        assert_eq!(
            hit(trusted, "9.9.9.1").await,
            StatusCode::TOO_MANY_REQUESTS,
            "same XFF must reuse the same bucket at trusted_proxies = 1"
        );

        // trusted = 0: the header is ignored → one shared bucket.
        let untrusted = app(0);
        assert_eq!(hit(untrusted.clone(), "9.9.9.1").await, StatusCode::OK);
        assert_eq!(
            hit(untrusted, "9.9.9.2").await,
            StatusCode::TOO_MANY_REQUESTS,
            "distinct XFF must still share the ip:unknown bucket at trusted_proxies = 0"
        );
    }

    /// #107: the socket-IP fallback reads the peer address from the
    /// `ConnectInfo<SocketAddr>` extension (installed by the serving
    /// make-service). Present → a per-IP bucket; absent → `ip:unknown`.
    #[test]
    fn extract_client_key_uses_socket_ip_from_connect_info() {
        let mut req = axum::http::Request::builder()
            .uri("/")
            .body(axum::body::Body::empty())
            .unwrap();
        req.extensions_mut()
            .insert(axum::extract::ConnectInfo(std::net::SocketAddr::from((
                [203, 0, 113, 7],
                4242,
            ))));
        assert_eq!(
            extract_client_key_with_config(&req, 0),
            "ip:203.0.113.7",
            "ConnectInfo socket IP must key the bucket when XFF is untrusted"
        );
    }

    /// #107: end-to-end pin that the serve path actually installs
    /// `ConnectInfo`. Without `into_make_service_with_connect_info` the
    /// handler sees no extension and the key collapses to `ip:unknown` —
    /// every header-less client sharing one bucket. This test fails on the
    /// old plain-`axum::serve` wire-up, which is exactly the shipped-binary
    /// gap an independent review caught.
    #[tokio::test]
    async fn serve_path_installs_connect_info_for_ip_fallback() {
        async fn key_handler(req: axum::extract::Request) -> String {
            extract_client_key_with_config(&req, 0)
        }
        let app = axum::Router::new().route("/key", get(key_handler));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind ephemeral port");
        let addr = listener.local_addr().expect("local_addr");
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(async move {
            crate::http::serve_with_graceful_shutdown(listener, app, async move {
                let _ = rx.await;
            })
            .await
        });

        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut stream = tokio::net::TcpStream::connect(addr).await.expect("connect");
        stream
            .write_all(b"GET /key HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .await
            .expect("write request");
        let mut body = String::new();
        stream
            .read_to_string(&mut body)
            .await
            .expect("read response");
        assert!(
            body.ends_with("ip:127.0.0.1"),
            "serve path must install ConnectInfo so the key is the socket IP, got: {body}"
        );

        let _ = tx.send(());
        server
            .await
            .expect("server task")
            .expect("serve returns Ok");
    }

    #[tokio::test]
    async fn prune_removes_full_buckets() {
        let limiter = RateLimiter::new(2, IDLE_REFILL_RATE);
        // Create entries and drain them (each check leaves 1 of 2 tokens).
        limiter.check("client-a").await;
        limiter.check("client-b").await;
        limiter.check("client-c").await;
        // Every bucket was used just now, so none has refilled yet.
        limiter.prune().await;
        assert_eq!(
            limiter.bucket_count().await,
            3,
            "recently used buckets are not idle, none should be removed"
        );

        // Once the refill window has passed every bucket is idle (projected
        // from the clock — the stored counter still reads `capacity - 1`).
        tokio::time::sleep(IDLE_WAIT).await;
        limiter.prune().await;
        assert_eq!(
            limiter.bucket_count().await,
            0,
            "buckets refilled to capacity should be pruned"
        );
    }

    /// Goal-292: rate_limits_rejected counter increments when rate-limit fires.
    #[tokio::test]
    async fn rate_limits_rejected_counter_increments() {
        let metrics = Arc::new(Metrics::default());
        // Capacity 0 means every request is rejected immediately.
        let limiter = RateLimiter::new(0, 0.0);

        let app = axum::Router::new()
            .route("/", get(|| async { "ok" }))
            .layer(axum::middleware::from_fn_with_state(
                (limiter, metrics.clone()),
                rate_limit_middleware,
            ));

        // First request — should get 429 and increment counter.
        let resp = app
            .clone()
            .oneshot(
                axum::extract::Request::builder()
                    .uri("/")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(
            metrics.rate_limits_rejected.load(Ordering::Relaxed),
            1,
            "rate_limits_rejected should increment to 1"
        );

        // Second request — counter increments to 2.
        let resp = app
            .oneshot(
                axum::extract::Request::builder()
                    .uri("/")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(
            metrics.rate_limits_rejected.load(Ordering::Relaxed),
            2,
            "rate_limits_rejected should increment to 2"
        );
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

    /// #107 regression: with NO trusted proxies (direct exposure), a
    /// client-forged `X-Forwarded-For` header must be IGNORED entirely —
    /// the previous leftmost-entry behaviour handed out a fresh full
    /// bucket per rotated header value, making the rate limit a no-op.
    /// The key must fall through to the socket-IP branch (absent in these
    /// fixtures → `ip:unknown`) regardless of what XFF claims.
    #[test]
    fn extract_client_key_ignores_forged_xff_without_trusted_proxies() {
        for forged in [
            "203.0.113.42, 10.0.0.1",
            "  203.0.113.42  ,  10.0.0.1  ",
            "attacker-chosen-1",
        ] {
            let req = build_request_with_xff(Some(forged));
            let key = extract_client_key(&req);
            assert_eq!(
                key, "ip:unknown",
                "XFF {forged:?} must not be trusted without trusted proxies"
            );
        }
    }

    /// NEW-HTTP-7 (behind a trusted proxy): each trusted hop appends exactly
    /// one entry (the address it saw), so the client address is the
    /// `trusted_proxies`-th entry counting from the RIGHT; everything further
    /// left was sent by the client and is discarded.
    #[test]
    fn extract_client_key_walks_xff_right_to_left_by_trust_depth() {
        // One proxy in front (nginx `$remote_addr`): a single entry — the
        // client the proxy saw.
        let req = build_request_with_xff(Some("203.0.113.42"));
        let key = extract_client_key_with_config(&req, 1);
        assert_eq!(key, "xff:203.0.113.42");

        // Same deployment, but the client prepended a forged hop: the
        // proxy-appended entry is still the rightmost one.
        let req = build_request_with_xff(Some("1.2.3.4, 203.0.113.42"));
        let key = extract_client_key_with_config(&req, 1);
        assert_eq!(key, "xff:203.0.113.42", "forged prefix must be ignored");

        // Two proxies: "<client as seen by proxy1>, <proxy1 as seen by
        // proxy2>" — the client sits second from the right.
        let req = build_request_with_xff(Some("203.0.113.42, 10.1.0.7"));
        let key = extract_client_key_with_config(&req, 2);
        assert_eq!(key, "xff:203.0.113.42");

        // A forged prefix lengthens the chain without changing the depth —
        // it must not shift the selected entry either.
        let req = build_request_with_xff(Some("6.6.6.6, 203.0.113.42, 10.1.0.7"));
        let key = extract_client_key_with_config(&req, 2);
        assert_eq!(key, "xff:203.0.113.42", "forged prefix must be ignored");
    }

    /// A proxy that emits a *separate* `X-Forwarded-For` field instead of
    /// appending to the client's must not leave the client-controlled field
    /// at the trusted index. Parsing flattens every field left-to-right, so
    /// the proxy-appended entry is still the rightmost in the flat chain.
    #[test]
    fn extract_client_key_flattens_multiple_xff_fields() {
        let req = Request::builder()
            .uri("/")
            // Client-controlled field first, proxy-appended field second.
            .header("x-forwarded-for", "1.2.3.4")
            .header("x-forwarded-for", "203.0.113.42")
            .body(axum::body::Body::empty())
            .unwrap();
        assert_eq!(
            extract_client_key_with_config(&req, 1),
            "xff:203.0.113.42",
            "the proxy-appended field must win over the client's"
        );

        // Two separate fields, two trusted hops: the client sits second from
        // the right across the flattened chain.
        let req = Request::builder()
            .uri("/")
            .header("x-forwarded-for", "6.6.6.6")
            .header("x-forwarded-for", "203.0.113.42")
            .header("x-forwarded-for", "10.1.0.7")
            .body(axum::body::Body::empty())
            .unwrap();
        assert_eq!(extract_client_key_with_config(&req, 2), "xff:203.0.113.42");
    }

    /// A chain shorter than the trusted-hop count is not trustworthy — some
    /// trusted proxy in the path did not append an entry. Fall through to the
    /// socket-IP branch instead of trusting a client-supplied entry.
    #[test]
    fn extract_client_key_falls_back_when_chain_shorter_than_trust() {
        // 1 entry, 2 trusted hops.
        let req = build_request_with_xff(Some("10.0.0.1"));
        let key = extract_client_key_with_config(&req, 2);
        assert!(
            key.starts_with("ip:"),
            "1-entry XFF must not satisfy trust depth 2, got {key}"
        );

        // 2 entries, 3 trusted hops.
        let req = build_request_with_xff(Some("203.0.113.42, 10.0.0.1"));
        let key = extract_client_key_with_config(&req, 3);
        assert!(
            key.starts_with("ip:"),
            "2-entry XFF must not satisfy trust depth 3, got {key}"
        );

        // Present-but-empty header likewise falls through.
        let req = build_request_with_xff(Some(""));
        let key = extract_client_key_with_config(&req, 1);
        assert!(
            key.starts_with("ip:"),
            "empty XFF should fall through to socket IP, got {key}"
        );
    }

    /// #107: an authenticated client's bucket key comes from its API key —
    /// NOT from any XFF value it sends. A direct client cannot escape its
    /// existing bucket by rotating forged XFF headers alongside the key.
    #[test]
    fn api_key_wins_over_xff_and_rotation_cannot_mint_new_buckets() {
        let mk = |xff: &str| {
            Request::builder()
                .header("x-api-key", "test-key-123")
                .header("x-forwarded-for", xff)
                .body(axum::body::Body::empty())
                .unwrap()
        };
        let key_a = extract_client_key(&mk("1.1.1.1"));
        let key_b = extract_client_key_with_config(&mk("2.2.2.2"), 1);
        assert!(
            key_a.starts_with("apikey:") && key_a == key_b,
            "API key must pin the bucket regardless of XFF: {key_a} vs {key_b}"
        );
        // An empty API-key header is not an identity: fall through.
        let req = Request::builder()
            .header("x-api-key", "")
            .body(axum::body::Body::empty())
            .unwrap();
        assert_eq!(
            extract_client_key(&req),
            "ip:unknown",
            "empty x-api-key must not create an apikey: bucket"
        );
    }

    /// Bucket-map cap (#107): a flood of unique keys is evicted back down once
    /// victims go idle, and idleness is derived from the clock — the stored
    /// counter alone never signals it. Buckets used within the refill window
    /// are never evicted, so the map can exceed the cap by the number of
    /// in-flight clients.
    #[tokio::test]
    async fn bucket_map_is_bounded_under_unique_key_flood() {
        let limiter = RateLimiter::new(2, IDLE_REFILL_RATE).with_max_buckets(4);
        // Fill the cap with four distinct clients, each drained by its request.
        for i in 0..4 {
            assert!(limiter.check(&format!("flood-{i}")).await);
        }
        assert_eq!(limiter.bucket_count().await, 4);
        // A 5th distinct client arrives while all four buckets are still
        // refilling: none is idle yet, so no victim exists and the insert is
        // allowed (the cap is a burst guard, not a hard ceiling).
        assert!(limiter.check("flood-4").await);
        assert_eq!(limiter.bucket_count().await, 5);

        // Once the refill window elapses every bucket is idle (no counter was
        // written — the state comes from the clock), so the next new key
        // reclaims one instead of growing the map.
        tokio::time::sleep(IDLE_WAIT).await;
        assert!(limiter.check("flood-5").await);
        assert_eq!(
            limiter.bucket_count().await,
            5,
            "an idle victim must be evicted so the map stops growing"
        );
        assert!(
            !limiter.buckets.lock().await.contains_key("flood-0"),
            "the least-recently-refilled idle bucket must be the eviction victim"
        );
    }

    /// #107 eviction preference: the header-derived (`xff:`) class is evicted
    /// before socket (`ip:`) keys, and credential (`apikey:`) keys are evicted
    /// last — so a header-rotating flood cannot crowd real clients out of a
    /// full map.
    #[tokio::test]
    async fn eviction_prefers_header_derived_keys_over_socket_and_authenticated() {
        let limiter = RateLimiter::new(2, IDLE_REFILL_RATE).with_max_buckets(3);
        assert!(limiter.check("apikey:aaa").await);
        assert!(limiter.check("ip:ccc").await);
        assert!(limiter.check("xff:bbb").await);
        // After the refill window ALL buckets are idle (clock-derived) → all
        // evictable.
        tokio::time::sleep(IDLE_WAIT).await;

        // New key #1: the header-derived bucket loses.
        assert!(limiter.check("apikey:zzz").await);
        {
            let b = limiter.buckets.lock().await;
            assert!(
                !b.contains_key("xff:bbb"),
                "header-derived key must be evicted first"
            );
            assert!(b.contains_key("apikey:aaa"));
            assert!(b.contains_key("ip:ccc"));
        }

        // New key #2: now the socket-IP bucket goes, not the authenticated
        // one. `apikey:aaa` and `ip:ccc` were never touched again, so they are
        // still idle; `apikey:zzz` was just used and is not.
        assert!(limiter.check("ip:ddd").await);
        let b = limiter.buckets.lock().await;
        assert!(
            !b.contains_key("ip:ccc"),
            "socket key must be evicted before the authenticated one"
        );
        assert!(b.contains_key("apikey:aaa"), "authenticated key survives");
        assert!(b.contains_key("apikey:zzz"));
    }
}
