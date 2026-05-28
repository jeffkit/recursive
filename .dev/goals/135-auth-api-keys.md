# Goal 135 — Authentication: API key middleware

**Roadmap**: Phase 17.2 — Authentication (part 1/2: API keys; JWT split out as g136)

**Design principle check**:
- Implemented as: new `auth_middleware` in `src/http.rs`, mirroring the
  existing `rate_limit_middleware` and `metrics_middleware`. Auth state
  is a tiny new `AuthConfig` struct alongside `RateLimiter`.
- ❌ Does NOT modify `agent.rs`, `runtime.rs`, or any non-http product code.
- ❌ Does NOT add any new dependency.

## Why

The HTTP server is currently unauthenticated — anyone who can reach the
listening port can drive the agent, consume LLM budget, and read session
state. ROADMAP-v4 17.2 calls for API key + JWT auth. This goal lands the
API key half: a constant-time-compared `X-API-Key` header check applied
as middleware to all routes except `/health` and `/metrics`.

JWT is deferred to a follow-up goal (g136) because it requires a new
dependency (`jsonwebtoken`), key management decisions, and a claim
schema — out of scope for a single tight goal.

### Naming rationale (READ THIS)

The env var that configures inbound API keys is **`RECURSIVE_HTTP_AUTH_KEYS`**
(plural, with `HTTP_AUTH_` infix). This is deliberately **not**
`RECURSIVE_API_KEYS`, because:

- `RECURSIVE_API_KEY` (singular) already exists as the **outbound**
  credential the agent uses to authenticate against an LLM provider
  (see `src/config.rs::Config::api_key`).
- `RECURSIVE_HTTP_AUTH_KEYS` (plural, prefixed with `HTTP_AUTH_`) is the
  **inbound** credential set the HTTP server accepts from clients.

These live at opposite ends of the network conversation; the names
should not be confusable. The prefix `RECURSIVE_HTTP_` also matches the
existing `RECURSIVE_RATE_LIMIT_*` env conventions for HTTP-server-only
configuration (see `rate_limiter_from_env` in `src/http.rs`).

## Scope (do exactly this, no more)

### 1. `AuthConfig` struct + env-driven constructor

In `src/http.rs`, add alongside `RateLimiter` (around line 87):

```rust
/// API key authentication. Empty key set = auth disabled (back-compat default).
#[derive(Clone, Default)]
pub struct AuthConfig {
    keys: Arc<Vec<String>>,
}

impl AuthConfig {
    pub fn new(keys: Vec<String>) -> Self {
        Self { keys: Arc::new(keys) }
    }

    /// Constant-time check whether `presented` is in the configured set.
    /// Returns `true` if auth is disabled (empty key set) — endpoints
    /// must rely on the middleware layering, not this method, for the
    /// "auth disabled" semantics.
    pub fn is_valid(&self, presented: &str) -> bool {
        if self.keys.is_empty() {
            return true;
        }
        // Constant-time comparison to avoid timing oracles. Loop over
        // every configured key regardless of early match.
        let mut found = false;
        let presented_bytes = presented.as_bytes();
        for k in self.keys.iter() {
            let k_bytes = k.as_bytes();
            if k_bytes.len() != presented_bytes.len() {
                continue;
            }
            let mut diff: u8 = 0;
            for (a, b) in k_bytes.iter().zip(presented_bytes.iter()) {
                diff |= a ^ b;
            }
            if diff == 0 {
                found = true;
            }
        }
        found
    }

    pub fn is_enabled(&self) -> bool {
        !self.keys.is_empty()
    }
}

/// Build `AuthConfig` from `RECURSIVE_HTTP_AUTH_KEYS` (comma-separated).
/// Returns the default (empty / disabled) if the env var is unset or empty.
fn auth_config_from_env() -> AuthConfig {
    let raw = std::env::var("RECURSIVE_HTTP_AUTH_KEYS").unwrap_or_default();
    let keys: Vec<String> = raw
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    AuthConfig::new(keys)
}
```

### 2. `auth_middleware` — header check, exempt paths

Add alongside `rate_limit_middleware` (around line 196):

```rust
async fn auth_middleware(
    State(auth): State<AuthConfig>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    // Auth disabled: pass through.
    if !auth.is_enabled() {
        return next.run(req).await;
    }
    // Exempt paths (k8s liveness probes + Prometheus scraping must work
    // without auth).
    let path = req.uri().path();
    if path == "/health" || path == "/metrics" {
        return next.run(req).await;
    }
    // Extract X-API-Key header.
    let presented = req
        .headers()
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if presented.is_empty() || !auth.is_valid(presented) {
        let mut resp = axum::response::Response::new(
            axum::body::Body::from("unauthorized"),
        );
        *resp.status_mut() = StatusCode::UNAUTHORIZED;
        return resp;
    }
    next.run(req).await
}
```

### 3. Wire it into `build_router`

`build_router` is at around line 356. Layer order matters in axum 0.8:
auth should run **before** rate-limit (no point burning rate-limit
budget on unauthenticated requests). axum applies layers in **reverse**
call order, so the auth layer is added **last** in the chain:

```rust
        .layer(axum::middleware::from_fn_with_state(
            state.metrics.clone(),
            metrics_middleware,
        ))
        .layer(axum::middleware::from_fn_with_state(
            limiter,
            rate_limit_middleware,
        ))
        .layer(axum::middleware::from_fn_with_state(
            auth_config_from_env(),
            auth_middleware,
        ))
        .with_state(Arc::new(state))
```

### 4. Tests in `tests/http.rs`

All inside `mod http_tests`, mirroring the metrics tests added in g134
(see `metrics_returns_prometheus_format` etc. near the end of the file).

To avoid env-var races (see `.dev/AGENTS.md`'s warning on parallel
tests touching `std::env::set_var`), introduce a private test helper
that builds a router with an explicit `AuthConfig`, instead of relying
on `RECURSIVE_HTTP_AUTH_KEYS`:

```rust
fn build_router_with_auth(state: AppState, auth: AuthConfig) -> Router {
    // Same as build_router but with explicit AuthConfig. Public
    // build_router stays env-driven; this is test-only.
}
```

The cleanest way: export a second public constructor in `src/http.rs`
(e.g. `build_router_with_auth(state, auth)`) that the tests can use.
The original `build_router(state)` becomes a thin wrapper:
`build_router_with_auth(state, auth_config_from_env())`. This makes
auth testable without env-var races and parallels the layering pattern
the rate limiter would have benefited from.

Tests (6 total):

- **Test A — `auth_disabled_passes_through`**: Build a router with
  `AuthConfig::default()` (empty key set). `GET /tools` returns 200
  without any header.

- **Test B — `auth_enabled_rejects_missing_header`**: Build a router
  with `AuthConfig::new(vec!["secret".into()])`. `GET /tools` without
  `X-API-Key` returns 401 with body `"unauthorized"`.

- **Test C — `auth_enabled_accepts_valid_key`**: Same setup. `GET /tools`
  with header `X-API-Key: secret` returns 200.

- **Test D — `auth_enabled_rejects_wrong_key`**: Same setup. `GET /tools`
  with `X-API-Key: bogus` returns 401.

- **Test E — `auth_health_and_metrics_are_exempt`**: With auth enabled
  (key set non-empty), `GET /health` and `GET /metrics` BOTH return 200
  without any header.

- **Test F — `auth_config_is_valid_unit`**: Pure unit test for
  `AuthConfig::is_valid`:
  - Empty config: any input (including empty string) returns true.
  - Populated config: correct key returns true; wrong key returns false;
    empty string returns false; key with off-by-one length returns false.

### 5. Document the env var in module-level docs

If `src/http.rs` already has a `//!` module doc, extend it with a short
paragraph describing `RECURSIVE_HTTP_AUTH_KEYS`. If it does not, do
NOT introduce one — instead add a `///` doc comment on `AuthConfig`
itself explaining the env var, the comma-separated format, the disabled
default, and the distinction from `RECURSIVE_API_KEY` (singular,
outbound).

## Acceptance

- `cargo build --features http` green.
- `cargo test --features http --test http` green; the 6 new tests pass.
- `cargo test --all-features` (full suite) green.
- `cargo fmt --all -- --check` clean.
- `cargo clippy --all-targets --all-features -- -D warnings` clean.
- Backward compatibility: starting the server with no
  `RECURSIVE_HTTP_AUTH_KEYS` env behaves exactly as today (no auth
  required, all existing tests pass).
- Files modified: `src/http.rs` (~120 lines added — `AuthConfig`,
  `auth_middleware`, `auth_config_from_env`, `build_router_with_auth`,
  rewire `build_router`), `tests/http.rs` (~100 lines added — 6 tests
  + helper). No other source files touched.
- No new dependency in `Cargo.toml`.
- `grep -rE 'RECURSIVE_API_KEYS' src/ tests/` returns nothing
  (i.e. we did not collide on the singular name).

## Notes for the agent

- `RateLimiter` (`src/http.rs:87`) and `rate_limit_middleware`
  (`src/http.rs:196`) are the structural template. Mirror their shape
  for `AuthConfig` and `auth_middleware`.
- `extract_client_key` (`src/http.rs:169`) shows the right pattern
  for reading `x-api-key` from headers (case-insensitive lookup via
  `headers().get("x-api-key")`).
- Layer order: in axum 0.8 `.layer()` applies in **reverse** call
  order. The `auth_middleware` layer must be added **last** in the
  chain so it executes **first**. See the worked example in §3.
- DO NOT use `std::env::set_var` in tests. Build an `AuthConfig`
  directly via the new `build_router_with_auth` helper. Tests run
  in parallel; env-var tests race.
- DO NOT bypass `auth.is_valid` with a fast-path on first match —
  the loop must always iterate over all configured keys to maintain
  constant time. The OR-of-EQ pattern in §1 is the reference.
- DO NOT add any new dep. JWT is a separate goal (g136).
- `AuthConfig` is `Clone` (Arc'd internally), so `from_fn_with_state`
  can take it by value without lifetimes.
- The 401 response body is plain text `unauthorized`, not JSON —
  matches the existing `rate_limit_middleware` style ("rate limit
  exceeded").
- The env name is `RECURSIVE_HTTP_AUTH_KEYS` (plural, with
  `HTTP_AUTH_` infix). DO NOT shorten it to `RECURSIVE_API_KEYS` —
  that name would collide visually with `RECURSIVE_API_KEY`
  (singular, the LLM credential), which is exactly the bug we are
  avoiding.
- After landing, update ROADMAP-v4 17.2: change 🔴 to 🟡 (partial; JWT
  pending) with a note pointing at g136.
