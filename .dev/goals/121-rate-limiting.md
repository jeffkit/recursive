# Goal 121 — Rate limiting middleware for HTTP API

**Roadmap**: Phase 17.1 — Rate limiting (per-session, per-API-key)

**Design principle check**:
- Implemented as: tower middleware layer in src/http.rs
- Stateless token-bucket per client IP (or API key header)
- ❌ Does NOT modify agent.rs or the core run loop

## Why

The HTTP API currently has no protection against abuse. A simple
token-bucket rate limiter prevents runaway clients from exhausting
LLM budget or server resources.

## Scope (do exactly this, no more)

### 1. Add rate limiter state to HTTP server

In `src/http.rs`, add a shared rate limiter:

```rust
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use std::time::Instant;

#[derive(Clone)]
struct RateLimiter {
    /// Tokens remaining per client key.
    buckets: Arc<Mutex<HashMap<String, TokenBucket>>>,
    /// Max tokens per bucket.
    capacity: u32,
    /// Tokens refilled per second.
    refill_rate: f64,
}

struct TokenBucket {
    tokens: f64,
    last_refill: Instant,
}

impl RateLimiter {
    fn new(capacity: u32, refill_rate: f64) -> Self { ... }
    async fn check(&self, key: &str) -> bool { ... }
}
```

### 2. Add as middleware layer

```rust
// In the router setup:
async fn rate_limit_middleware(
    State(limiter): State<RateLimiter>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let key = extract_client_key(&req);
    if !limiter.check(&key).await {
        return (StatusCode::TOO_MANY_REQUESTS, "rate limit exceeded").into_response();
    }
    next.run(req).await
}
```

### 3. Configuration via environment

- `RECURSIVE_RATE_LIMIT_RPM` — requests per minute (default: 60)
- `RECURSIVE_RATE_LIMIT_BURST` — burst capacity (default: 10)

### 4. Tests

- **Test A**: Requests within limit succeed (200)
- **Test B**: Requests exceeding limit get 429
- **Test C**: Tokens refill after waiting
- **Test D**: Different clients have independent buckets

## Acceptance

- `cargo build --features http` green.
- `cargo test` green.
- `cargo clippy --all-targets --all-features -- -D warnings` clean.
- Only `src/http.rs` is modified.
- Sending > 60 requests/min to the API returns 429 Too Many Requests.

## Notes for the agent

- The HTTP server uses `axum` with `tower` layers.
- Look for the router creation (e.g., `Router::new()`) to add the middleware.
- Use `axum::middleware::from_fn_with_state` to wire the middleware.
- The client key should be: API key header if present, else remote IP.
- Do NOT add external rate-limiting crates. Implement a simple token bucket.
- Do NOT modify any file other than `src/http.rs`.
- The HTTP feature is gated behind `#[cfg(feature = "http")]`.
