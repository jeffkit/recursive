# Goal 272 — HTTP auth bypass: route-level merge

**Roadmap**: Phase 17 (Production Hardening) — P1 from
`docs/review/architecture-review-2026-06-10.md` (NEW-HTTP-6)

**Design principle check**:
- Implemented as: split the router into a "public" sub-router
  and a "protected" sub-router, merge them at the top level
- ❌ Does NOT branch inside `agent.rs::Agent::run`'s main loop
- ❌ Does NOT add a new feature flag

## Why

`auth.rs:200` hard-codes the auth bypass list:

```rust
if path == "/health" || path == "/metrics" {
    return next.run(req).await;
}
```

When `http/mod.rs:421` added `GET /openapi.json`, the bypass
list wasn't updated — that endpoint now requires auth even
though its content is intentionally public (the OpenAPI spec
itself does not contain secrets). Every new public route is
the same trap: maintainer adds the route, forgets the bypass
list, ships, sees a surprise 401.

The fix is to make "this route is public" a property of the
route itself, not a side-table in auth.rs. axum supports
sub-router composition via `Router::merge` — we put public
routes in one sub-router (no auth layer), protected routes
in another, and merge at the top.

## Scope (do exactly this, no more)

### 1. Split the router in `src/http/mod.rs`

Read the current router around line 396-422. Identify the
existing routes. Restructure as:

```rust
// Public sub-router: no auth layer
let public = Router::new()
    .route("/health", get(health))
    .route("/metrics", get(metrics_handler))
    .route("/openapi.json", get(openapi_spec));

// Protected sub-router: auth + rate-limit applied
let protected = Router::new()
    .route("/run", post(run_agent))
    .route("/agui", post(agui_run))
    .route("/sessions", post(create_session))
    /* ... all other routes ... */
    .layer(/* auth middleware */)
    .layer(/* rate-limit middleware */);

// Top-level: merge public + protected
let app = Router::new()
    .merge(public)
    .merge(protected)
    .with_state(state);
```

The exact middlewares currently applied to the unified router
(auth, rate-limit, body-limit, trace, CORS) must be applied
to the **protected** sub-router only, NOT to public. Public
routes get only the body-limit and trace layers (or no
middleware at all if the body-limit is already on the top
router).

### 2. Remove the string-comparison bypass in `auth.rs`

In `src/http/auth.rs`, change the `auth_middleware` function
to **always** call `next.run(req).await` — no more
`if path == "/health" ...` short-circuit. The function
should be ~10 lines shorter as a result.

If a `path == "/openapi.json"` is referenced elsewhere, also
remove it. The intent of this goal is to make the bypass
**structural** (lives in the router), not textual (lives in
auth.rs).

### 3. Tests in `src/http/mod.rs` `#[cfg(test)] mod tests`

Add a test that asserts the auth bypass is **structural**:

```rust
#[tokio::test]
async fn public_routes_bypass_auth() {
    // Build a minimal app state.
    // Hit GET /health, /metrics, /openapi.json — each must
    // succeed without an Authorization header.
    // Asserting "no auth required" is the entire point.
}

#[tokio::test]
async fn protected_routes_require_auth() {
    // Hit POST /run, POST /agui, etc. without an Authorization
    // header — each must return 401.
}
```

If the existing test harness can't easily spin up a Router
without the full AppState, write a source-grep snapshot test
that asserts:
- `http/mod.rs` contains `Router::new().route("/health", ...)`
  in a context that does NOT have auth middleware applied
- `auth.rs` no longer contains the strings `"/health"` or
  `"/metrics"`

A source-grep snapshot is acceptable per the lead override
documented in g268's journal entry.

### 4. No changes outside `src/http/auth.rs` and `src/http/mod.rs`

Do not touch any other file. This is a structural refactor
of the router + a small deletion in auth.rs.

## Acceptance

- `cargo test --workspace` — green (existing + new test)
- `cargo clippy --all-targets --all-features -- -D warnings` —
  clean
- `cargo fmt --all` — applied
- `grep "path == .*health" src/http/auth.rs` — 0 matches
- `grep "path == .*metrics" src/http/auth.rs` — 0 matches
- `grep "path == .*openapi" src/http/auth.rs` — 0 matches
- A new test in `src/http/mod.rs` exercises the public-route
  bypass (or a source-grep snapshot if runtime is too heavy)

## Notes for the agent

- The axum `Router::merge` is the right primitive. Do NOT
  use `Router::nest` for this — `nest` adds a path prefix
  and we want flat routes. `merge` is what the goal
  description calls for.
- The body-limit layer (`DefaultBodyLimit::max(1MB)`,
  recently added per review) should stay on the top router
  so it applies to both sub-routers. Only the auth and
  rate-limit layers should move to the protected sub-router.
- If you find an integration test that builds the Router
  directly (likely `tests/http.rs`), update it to use the
  new merge structure.
- Do NOT add a new module. Stay in `mod.rs` and `auth.rs`.
- Estimated diff: 2 files, ~30-60 lines net (router split +
  bypass deletion + 1-2 tests).
- **Test discipline reminder (from g268 post-mortem)**: do
  NOT call `runtime.run()` or use `MockProvider` in a
  blocking test. Use a direct `tower::ServiceExt::oneshot`
  on the Router with a minimal `Request`, OR use the
  source-grep snapshot form if setup is too heavy.
