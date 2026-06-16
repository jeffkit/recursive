# Goal 292 — Add sessions_active and rate_limits_rejected to /metrics

**Roadmap**: Post-Phase (observability improvement)

**Design principle check**:
- Implemented as: add two fields to `Metrics`, wire rate-limit middleware to
  increment the rejection counter, expose both in `metrics_handler`.
- ❌ Does NOT branch inside `agent.rs::Agent::run`'s main loop

## Why

The `/metrics` endpoint (Prometheus format) is missing two operationally
important observability signals:

1. **`recursive_sessions_active`** (gauge): how many sessions currently exist
   in `AppState::sessions`. Operators cannot tell if the server is accumulating
   sessions without calling `GET /sessions` and counting them manually.

2. **`recursive_rate_limits_rejected_total`** (counter): how many requests have
   been rate-limited (HTTP 429). The `rate_limit_middleware` currently returns
   429 without incrementing any counter, so there is no way to detect a DDoS
   or misconfigured rate limit from Prometheus.

## Scope (do exactly this, no more)

### 1. `src/http/mod.rs` — add two fields to `Metrics`

```rust
pub struct Metrics {
    // ... existing fields ...
    /// Number of currently open sessions (gauge).
    pub sessions_active: AtomicU64,
    /// Number of requests rejected by rate limiting (counter).
    pub rate_limits_rejected: AtomicU64,
}
```

Update `Metrics::default()` (or `Metrics { ..Default::default() }`) to
initialise both with `AtomicU64::new(0)`.

### 2. `src/http/mod.rs` — maintain `sessions_active` in session CRUD

Find the places in `handlers.rs` where sessions are inserted and removed:
- Insert: `state.sessions.write().await.insert(...)` → add
  `state.metrics.sessions_active.fetch_add(1, Ordering::Relaxed)` after.
- Remove (session_reaper): `sessions.remove(...)` → add
  `state.metrics.sessions_active.fetch_sub(1, Ordering::Relaxed)` after.

There may be 2–3 insertion sites (`create_session`, `create_session_from_preset`)
and 1 removal site (the reaper). Read the handlers to confirm.

### 3. `src/http/rate_limit.rs` — pass `Metrics` to rate-limit middleware

Currently `rate_limit_middleware` takes `State(limiter): State<RateLimiter>`.
Change the state type to a tuple `(RateLimiter, Arc<Metrics>)` or add a
second `State` extractor.

Simplest approach — add `Arc<Metrics>` to the state:

```rust
pub(super) async fn rate_limit_middleware(
    State((limiter, metrics)): State<(RateLimiter, Arc<Metrics>)>,
    req: Request,
    next: Next,
) -> Response {
    let key = extract_client_key(&req);
    if !limiter.check(&key).await {
        metrics.rate_limits_rejected.fetch_add(1, Ordering::Relaxed);
        let mut resp = Response::new(Body::from("rate limit exceeded"));
        *resp.status_mut() = StatusCode::TOO_MANY_REQUESTS;
        return resp;
    }
    next.run(req).await
}
```

Update `build_router_with_auth_and_rate_limit` to pass `(limiter.clone(),
state_arc.metrics.clone())` to the layer.

### 4. `src/http/handlers.rs` — expose in `metrics_handler`

Append two new metric blocks in `metrics_handler`:

```
# HELP recursive_sessions_active Currently active sessions
# TYPE recursive_sessions_active gauge
recursive_sessions_active {sessions_active}
# HELP recursive_rate_limits_rejected_total Total requests rejected by rate limiting
# TYPE recursive_rate_limits_rejected_total counter
recursive_rate_limits_rejected_total {rate_limits_rejected}
```

### 5. Tests

Update any existing test that constructs `AppState` or `Metrics` to include
the new fields. Add at minimum:

- A test asserting `rate_limits_rejected` increments when rate-limit fires.
- A test asserting `sessions_active` increments on session creation and
  decrements on reaper eviction.

## Acceptance

- `cargo test --workspace` green
- `cargo clippy --all-targets --all-features -- -D warnings` clean
- `cargo fmt --all` clean
- `/metrics` output contains `recursive_sessions_active` and
  `recursive_rate_limits_rejected_total`
- `rate_limits_rejected` increments when `rate_limit_middleware` returns 429

## Notes for the agent

- Read `src/http/mod.rs` (Metrics struct, build_router_with_auth_and_rate_limit)
  and `src/http/rate_limit.rs` (rate_limit_middleware) first.
- Read `src/http/handlers.rs` (create_session, session_reaper, metrics_handler).
- The state tuple approach `(RateLimiter, Arc<Metrics>)` is the simplest way
  to thread metrics through the rate-limit middleware.
- Do NOT add the `prometheus` crate or any other metrics library — the
  existing text-format output is sufficient.
- **DO NOT modify** `src/agent.rs`, `src/runtime.rs`, `src/kernel.rs`,
  `src/run_core.rs`.
