# Goal 122 — Prometheus-compatible metrics HTTP endpoint

**Roadmap**: Phase 15.2 — Metrics endpoint

**Design principle check**:
- Implemented as: `/metrics` endpoint in src/http.rs
- Exposes counters/gauges in Prometheus text format
- ❌ Does NOT modify agent.rs

## Why

Production deployments need machine-readable metrics for monitoring.
A `/metrics` endpoint in Prometheus exposition format lets standard
tooling (Prometheus, Grafana, Datadog) scrape agent health data.

## Scope (do exactly this, no more)

### 1. Add metrics state struct

In `src/http.rs`, add a shared metrics collector:

```rust
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Default)]
pub struct Metrics {
    pub requests_total: AtomicU64,
    pub requests_active: AtomicU64,
    pub agent_runs_total: AtomicU64,
    pub agent_runs_success: AtomicU64,
    pub agent_runs_failed: AtomicU64,
    pub tokens_prompt_total: AtomicU64,
    pub tokens_completion_total: AtomicU64,
    pub agent_steps_total: AtomicU64,
}
```

### 2. Add `/metrics` endpoint

```rust
async fn metrics_handler(State(metrics): State<Arc<Metrics>>) -> String {
    format!(
        "# HELP recursive_requests_total Total HTTP requests\n\
         # TYPE recursive_requests_total counter\n\
         recursive_requests_total {}\n\
         # HELP recursive_agent_runs_total Total agent runs\n\
         # TYPE recursive_agent_runs_total counter\n\
         recursive_agent_runs_total {}\n\
         ...",
        metrics.requests_total.load(Ordering::Relaxed),
        metrics.agent_runs_total.load(Ordering::Relaxed),
    )
}
```

### 3. Increment metrics at appropriate points

- `requests_total`: increment in the request middleware
- `agent_runs_*`: increment when a run completes/fails
- `tokens_*`: increment from agent outcome

### 4. Tests

- **Test A**: GET /metrics returns 200 with prometheus format
- **Test B**: Metrics increment after requests
- **Test C**: Counter values persist across requests

## Acceptance

- `cargo build --features http` green.
- `cargo test` green.
- `curl localhost:3000/metrics` returns Prometheus text format.
- Only `src/http.rs` is modified.
- No external metrics crate added (pure atomic counters).

## Notes for the agent

- Use `std::sync::atomic` for lock-free counters. No external crate needed.
- The `/metrics` endpoint should NOT be rate-limited (exclude it from
  the rate limiter if Goal 121 has landed).
- Prometheus text format: `metric_name value\n` with optional TYPE/HELP comments.
- Add the metrics as shared state alongside the existing app state.
- Do NOT modify any file other than `src/http.rs`.
- Look at how the existing routes are structured and follow the same pattern.
