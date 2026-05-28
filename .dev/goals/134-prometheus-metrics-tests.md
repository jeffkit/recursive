# Goal 134 — Tests for `/metrics` Prometheus endpoint

**Roadmap**: Phase 15.2 — Metrics endpoint (test coverage; implementation already shipped in Goal 122 / commit `01792b7`)

**Design principle check**:
- Implemented as: new `#[tokio::test]` tests inside `tests/http.rs` `http_tests` module
- ❌ Does NOT modify `src/http.rs` (implementation is correct as of HEAD)
- ❌ Does NOT modify `agent.rs` or any other product code

## Why

Goal 122 shipped a complete Prometheus-compatible `/metrics` endpoint
in `src/http.rs`: a `Metrics` struct of 8 atomic counters, a
`metrics_handler`, an auto-incrementing `metrics_middleware`, and
counter increments inside the agent-run handler. None of it has any
test coverage — `grep recursive_(requests|agent_runs|tokens)` across
`tests/` returns zero hits. This goal closes that gap.

This is the last 🔴 → ✅ for ROADMAP-v4 Phase 15.2.

## Scope (do exactly this, no more)

### 1. Add `/metrics` tests to `tests/http.rs`

All tests live inside the existing `mod http_tests` block. Reuse
`sample_state()` and `build_router(...)` exactly like
`health_returns_ok` (line 87-104) does.

#### Test A — endpoint responds with Prometheus exposition format

```rust
#[tokio::test]
async fn metrics_returns_prometheus_format() {
    let app = build_router(sample_state());

    let response = app
        .oneshot(
            axum::http::Request::builder()
                .uri("/metrics")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let text = std::str::from_utf8(&body).unwrap();

    // Must contain HELP/TYPE preambles for at least one counter and one gauge.
    assert!(text.contains("# HELP recursive_requests_total"));
    assert!(text.contains("# TYPE recursive_requests_total counter"));
    assert!(text.contains("# TYPE recursive_requests_active gauge"));

    // Must list every metric name from the Metrics struct.
    for name in [
        "recursive_requests_total",
        "recursive_requests_active",
        "recursive_agent_runs_total",
        "recursive_agent_runs_success",
        "recursive_agent_runs_failed",
        "recursive_tokens_prompt_total",
        "recursive_tokens_completion_total",
        "recursive_agent_steps_total",
    ] {
        assert!(text.contains(name), "missing metric: {name}");
    }
}
```

#### Test B — middleware increments `requests_total`

The `metrics_middleware` (src/http.rs L182) wraps every routed
request. Hitting any endpoint should bump `requests_total`. Use the
shared `AppState` so the same `Arc<Metrics>` is observed by handler
and middleware.

```rust
#[tokio::test]
async fn metrics_middleware_increments_requests_total() {
    let state = sample_state();
    let metrics = state.metrics.clone();
    let app = build_router(state);

    // Hit two non-/metrics endpoints to drive the middleware.
    for uri in ["/health", "/tools"] {
        let _ = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri(uri)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
    }

    let n = metrics
        .requests_total
        .load(std::sync::atomic::Ordering::Relaxed);
    assert!(n >= 2, "expected requests_total >= 2, got {n}");
}
```

Note: `/metrics` itself is also wrapped by the middleware; the
assertion uses `>= 2` to stay robust if the agent (or you, when
debugging) adds more requests.

#### Test C — counter values render correctly after manual increment

This pins the round-trip: write into `Metrics` directly → handler
emits the new value in the response body. Avoids needing to drive a
full agent run.

```rust
#[tokio::test]
async fn metrics_counter_values_render() {
    let state = sample_state();
    state
        .metrics
        .agent_runs_total
        .store(7, std::sync::atomic::Ordering::Relaxed);
    state
        .metrics
        .tokens_prompt_total
        .store(12345, std::sync::atomic::Ordering::Relaxed);
    let app = build_router(state);

    let response = app
        .oneshot(
            axum::http::Request::builder()
                .uri("/metrics")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    let body = response.into_body().collect().await.unwrap().to_bytes();
    let text = std::str::from_utf8(&body).unwrap();
    assert!(text.contains("recursive_agent_runs_total 7"));
    assert!(text.contains("recursive_tokens_prompt_total 12345"));
}
```

## Acceptance

- `cargo build --features http` green.
- `cargo test --features http` green; the three new tests pass.
- `cargo clippy --all-targets --all-features -- -D warnings` clean.
- `cargo fmt --all` is a no-op.
- Only `tests/http.rs` is modified. **No changes to `src/`.**

## Notes for the agent

- The test module `http_tests` already imports
  `recursive::http::{build_router, AppState, Metrics, ...}` —
  no new imports needed for `Metrics`.
- The `sample_state()` helper at line 34 already constructs an
  `AppState` with `metrics: Arc::new(Metrics::default())`. Reuse it.
- Use `Body`, `BodyExt`, and `tower::ServiceExt::oneshot` exactly
  like `health_returns_ok` (line 87) — same imports, same shape.
- `AtomicU64` lives at `std::sync::atomic::{AtomicU64, Ordering}`.
  The fields are `pub` on `Metrics`, so direct `.load()` / `.store()`
  / `.fetch_add()` from the test is fine.
- DO NOT modify `src/http.rs`. The implementation is already correct.
  If you discover an issue, write it in the final message — do not
  silently fix it.
- DO NOT add any new dependency. `tower`, `http_body_util`, and
  `axum` are already pulled in by the existing tests.
- `clone()` the `Arc<Metrics>` before passing the state into
  `build_router`, otherwise the test loses access. See Test B.
- All three tests must use `#[tokio::test]` and `.await` on
  `oneshot`. They must be inside `mod http_tests` (the
  `#[cfg(feature = "http")]` block at the top of `tests/http.rs`).
- Keep the goal scope tight: this is a tests-only goal. If you find
  yourself editing files outside `tests/http.rs`, stop and re-read
  this section.
