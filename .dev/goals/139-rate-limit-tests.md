# Goal 139 — Rate limiting: integration test coverage + injection seam

**Roadmap**: Phase 17.1 — Rate limiting (test coverage; implementation
already shipped in Goal 121, commit 1d7a53d)

**Design principle check**:
- Implemented as: new `pub fn build_router_with_auth_and_rate_limit`
  in `src/http.rs`, parallel to `build_router_with_auth` (g135).
  `RateLimiter::new` becomes `pub`. Existing `build_router` and
  `build_router_with_auth` keep their env-driven defaults.
- ❌ Does NOT modify `agent.rs`, `runtime.rs`, or any non-http code.
- ❌ Does NOT add any new dependency.

## Why

g121 landed `RateLimiter` and `rate_limit_middleware` with seven
unit tests of the limiter logic itself (token-bucket math, env
parsing, client-key extraction). None of them go through
`axum::Router` — they call `RateLimiter.check()` directly. An
`rg '429|TOO_MANY_REQUESTS' tests/` returns nothing.

This goal closes the integration gap: when a real HTTP request
exceeds the configured rate, does the layered middleware actually
return 429? Symmetrically with what g135 did for auth.

g135's journal flagged "Rate limiter test seam: parallel to
`build_router_with_auth`" as a follow-up. This goal does that.

## Scope (do exactly this, no more)

### 1. Make `RateLimiter::new` public

In `src/http.rs:222`, change `fn new(...)` → `pub fn new(...)`.
The struct is already `pub`; the constructor was the only thing
gating external construction.

### 2. Add `build_router_with_auth_and_rate_limit`

```rust
pub fn build_router_with_auth_and_rate_limit(
    state: AppState,
    auth: AuthConfig,
    limiter: RateLimiter,
) -> Router { ... }
```

Body identical to `build_router_with_auth` except the `limiter`
parameter replaces the inline `rate_limiter_from_env()` call. The
existing `build_router_with_auth` becomes a thin wrapper that
calls the new function with `rate_limiter_from_env()`.

`build_router(state)` continues to wrap
`build_router_with_auth(state, auth_config_from_env())`.

### 3. Tests in `tests/http.rs`

All inside `mod http_tests`, mirroring the auth tests added in g135.

- **Test A — `rate_limit_first_request_succeeds`**: build router
  with capacity=2 limiter, hit `/health` once, expect 200.
- **Test B — `rate_limit_burst_allowed_then_429`**: capacity=2,
  refill very slow. Hit `/tools` twice → 200. Third hit → 429
  with body `"rate limit exceeded"`.
- **Test C — `rate_limit_different_clients_have_independent_buckets`**:
  capacity=1. Client A hits `/tools` with `X-API-Key: alpha` (200).
  Client B hits `/tools` with `X-API-Key: beta` (200). Client A's
  second hit → 429 (bucket exhausted) but Client B's second hit
  with a different key still hits its own bucket (depends on
  whether the second beta hit also exhausts; assert: A's second
  hit is 429 while B's first hit is 200, demonstrating
  independence).
- **Test D — `rate_limit_does_not_block_below_threshold`**: high
  capacity (e.g. 100). Send 5 requests. All return 200.

### 4. Documentation

Update the doc comment on `RateLimiter::new` to mention that
external callers (tests, custom embedders) can construct it
directly.

## Acceptance

- `cargo build --features http` green.
- `cargo test --features http --test http` green; new tests pass.
- `cargo test --all-features` (full suite) green.
- `cargo fmt --all -- --check` clean.
- `cargo clippy --all-targets --all-features -- -D warnings` clean.
- Backward compatibility: server started without
  `RECURSIVE_RATE_LIMIT_*` env behaves exactly as today.
- Files modified: `src/http.rs` (~30 lines added), `tests/http.rs`
  (~80 lines added). No other source files touched.
- No new dependency in `Cargo.toml`.

## Notes

- `RateLimiter` is `Clone` (Arc'd internally) — pass by value to
  the new constructor, just like `AuthConfig`.
- Tests should not rely on `RECURSIVE_RATE_LIMIT_*` env (parallel
  cargo test threads share process env). The new injection seam
  is the whole point.
- For Test B, set the refill rate to something tiny (e.g. 0.001/s)
  so the 100ms-or-so test runtime can't refill the bucket and
  perturb the assertion.
- Existing 7 unit tests of `RateLimiter::check` / env-parsing /
  client-key-extraction stay untouched — they cover the lower
  layer this goal does not re-test.
