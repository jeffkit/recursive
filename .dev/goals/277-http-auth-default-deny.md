# Goal 277 — HTTP default-deny when no auth configured

**Roadmap**: Phase 17 (Production Hardening) — drift P0 from
`docs/review/architecture-review-2026-06-10.md` (SEC-003) and
2026-06-15 review. This is the third review cycle this issue
has appeared; it needs an explicit owner + acceptance criterion.

**Design principle check**:
- Implemented as: change `auth_middleware` to return 503 when
  `is_enabled()` is false, instead of `next.run(req).await`.
  Update `AuthConfig::is_enabled()` to remain as-is (used by
  tests). Add an explicit `RECURSIVE_HTTP_AUTH_INSECURE_OK=1`
  opt-in for local dev.
- ❌ Does NOT branch inside `agent.rs::Agent::run`'s main loop
- ❌ Does NOT add a new feature flag (the env var is a debug
  escape hatch, not a runtime feature toggle)

## Why

`src/http/auth.rs:62-65` and `auth.rs:88`:

```rust
pub fn is_valid(&self, presented: &str) -> bool {
    if self.keys.is_empty() {
        return false;
    }
    ...
}

pub fn is_enabled(&self) -> bool {
    !self.keys.is_empty() || self.jwt.is_some()
}
```

And `auth.rs:197-199`:

```rust
if !auth.is_enabled() {
    return next.run(req).await;  // <-- bypass all auth
}
```

If the operator starts the HTTP server without setting
`RECURSIVE_HTTP_AUTH_KEYS`, `is_enabled()` returns false, and
`auth_middleware` passes every request through. This is the
default in `examples/`, in the README quick-start, and in any
container that doesn't explicitly configure auth. Operators
who deploy Recursive behind a public load balancer without
realizing this have an *unauthenticated remote code execution*
surface — every `/run` endpoint is wide open.

The 06-10 review flagged this; nothing changed. The 06-15 review
flagged it again. This goal is the third attempt; we add an
explicit opt-in env var to keep the local dev loop working
without making the default unsafe.

## Scope (do exactly this, no more)

### 1. Change `auth_middleware` default

In `src/http/auth.rs:197-199`, replace the bypass:

```rust
pub(super) async fn auth_middleware(
    axum::extract::State(auth): axum::extract::State<AuthConfig>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    // If auth is not configured, refuse by default. The only
    // way to run with no auth is the explicit debug opt-in.
    if !auth.is_enabled() {
        if !std::env::var("RECURSIVE_HTTP_AUTH_INSECURE_OK")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false)
        {
            tracing::error!(
                "HTTP server is running with NO auth configured. \
                 Set RECURSIVE_HTTP_AUTH_KEYS=<comma-separated-keys> \
                 or RECURSIVE_HTTP_AUTH_JWT_SECRET=<secret>. \
                 To override for local dev only, set \
                 RECURSIVE_HTTP_AUTH_INSECURE_OK=1."
            );
            let mut resp = axum::response::Response::new(axum::body::Body::from(
                "auth not configured; set RECURSIVE_HTTP_AUTH_KEYS or \
                 RECURSIVE_HTTP_AUTH_INSECURE_OK=1 (local dev only)",
            ));
            *resp.status_mut() = StatusCode::SERVICE_UNAVAILABLE;
            return resp;
        }
        tracing::warn!(
            "RECURSIVE_HTTP_AUTH_INSECURE_OK=1 set — bypassing auth. \
             This must NEVER be used in production."
        );
    }
    // ... rest unchanged
}
```

### 2. Document the env var in `auth_config_from_env`

In `src/http/auth.rs::auth_config_from_env`, when no keys are
configured AND no JWT is configured, emit the error log at
startup *once* (so operators don't have to make a request to
notice):

```rust
pub fn auth_config_from_env() -> AuthConfig {
    let config = AuthConfig::from_env();
    if !config.is_enabled() {
        tracing::error!(
            "HTTP auth is NOT configured. Set \
             RECURSIVE_HTTP_AUTH_KEYS=... or RECURSIVE_HTTP_AUTH_JWT_SECRET=... \
             to enable. For local dev only, set \
             RECURSIVE_HTTP_AUTH_INSECURE_OK=1 to bypass (NEVER in production)."
        );
    }
    config
}
```

### 3. Update tests

Existing tests in `src/http/auth.rs` and `tests/http.rs` that
expect "no auth → bypass" must be updated to set
`RECURSIVE_HTTP_AUTH_INSECURE_OK=1` explicitly, OR configure a
key. Pick whichever is less invasive per test — most tests
already set keys.

Add a new test:

```rust
#[tokio::test]
async fn no_auth_returns_503_by_default() {
    // Ensure env is unset for this test (use IsolatedEnv pattern
    // from .dev/AGENTS.md env-var guidance — single combined test
    // for all auth env var toggles).
    let auth = AuthConfig {
        keys: vec![],
        jwt: None,
    };
    let req = Request::builder().uri("/run").body(Body::empty()).unwrap();
    let resp = auth_middleware(State(auth), req, Next::new(/* 503-asserting svc */)).await;
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
}
```

### 4. Update README + docs

Update the README's "Quickstart: HTTP server" section to call
out the env var explicitly. Also update `docs/review/` references
to mark this issue as addressed in 0.7.0 release notes.

(README/docs changes are documentation, not a feature flag — they
ship with this PR.)

## Acceptance

- `cargo test --workspace` — green (existing + new test)
- `cargo clippy --all-targets --all-features -- -D warnings` —
  clean
- `cargo fmt --all` — applied
- `grep "RECURSIVE_HTTP_AUTH_INSECURE_OK" src/` — 3+ matches
  (middleware check, auth_config_from_env log, test)
- A fresh `cargo run -- serve --bind 0.0.0.0:8080` without env
  vars returns 503 on `/run` and emits the error log at startup.
- A fresh `cargo run -- serve --bind 0.0.0.0:8080` with
  `RECURSIVE_HTTP_AUTH_INSECURE_OK=1` accepts requests without
  auth and emits the warn log.
- A fresh `cargo run -- serve --bind 0.0.0.0:8080` with
  `RECURSIVE_HTTP_AUTH_KEYS=k1,k2` accepts requests with
  `X-API-Key: k1` and rejects others.

## Notes for the agent

- This is a **breaking behavior change**. Anyone running
  Recursive's HTTP server without explicit auth will see their
  requests rejected. That is the point, but it goes in a
  CHANGELOG entry and a release note.
- Do NOT remove `is_enabled()` — internal tests and the bypass
  check still need it.
- The env-var test must be ONE test, not many
  (`std::env::set_var` is process-global). See .dev/AGENTS.md
  invariant lesson about env-var tests.
- Estimated diff: 1 file (auth.rs) + tests + README +
  CHANGELOG. ~50 lines net.
- **Test discipline reminder (from g268 post-mortem)**: this
  goal's tests can use direct `auth_middleware` invocation
  with `tower::ServiceExt::oneshot` against a stub service.
  Don't spin up a real Router.

**Disjoint file guarantee**: This goal touches src/http/auth.rs,
README.md, CHANGELOG.md. Goals 274/275/276 don't touch
auth.rs. Safe to run in parallel.