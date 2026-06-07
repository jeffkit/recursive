# Goal 252 — Add concurrency limit to HTTP /run endpoint

**Roadmap**: Arch-review bugfixes (P1 — resource exhaustion)

**Design principle check**:
- Implemented as: `Arc<Semaphore>` in `AppState`, acquired by `/run` and `/sessions/:id/message` handlers
- ❌ Does NOT branch inside `agent.rs::Agent::run`'s main loop

## Why

The HTTP `/run` endpoint (`src/http/handlers.rs`) spins up a fresh
`AgentRuntime` per request with no limit on simultaneous in-flight runs.
Under load this creates N concurrent agent runs, each consuming LLM quota,
shell subprocess capacity, and transcript memory. The rate limiter
(`rate_limit.rs`) caps request rate but not simultaneous in-flight agents.

A `tokio::sync::Semaphore` with a configurable limit is the correct fix.

## Scope (do exactly this, no more)

### 1. `src/config.rs` — add `max_concurrent_runs: usize` to `Config`

Add a field with default `8`:
```rust
/// Maximum number of agent turns running concurrently in HTTP mode.
/// 0 means unlimited (not recommended for production).
#[serde(default = "default_max_concurrent_runs")]
pub max_concurrent_runs: usize,
```

Add the default function:
```rust
fn default_max_concurrent_runs() -> usize { 8 }
```

### 2. `src/http/mod.rs` — add semaphore to `AppState`

Add a field to `AppState`:
```rust
/// Limits simultaneous in-flight agent runs.
pub run_semaphore: Arc<tokio::sync::Semaphore>,
```

In the `AppState` construction site (wherever `AppState { ... }` is built,
likely in `src/main.rs` or `src/http/mod.rs`), initialize it:
```rust
run_semaphore: Arc::new(tokio::sync::Semaphore::new(config.max_concurrent_runs.max(1))),
```

### 3. `src/http/handlers.rs` — acquire semaphore permit in `run_handler` and `session_message_handler`

In the `run_handler` function (the `POST /run` handler), acquire a permit
before creating the `AgentRuntime`:

```rust
let _permit = state
    .run_semaphore
    .acquire()
    .await
    .map_err(|_| (StatusCode::SERVICE_UNAVAILABLE, Json(json!({
        "error": "server shutting down"
    }))))?;
```

Do the same in `session_message_handler` (the `POST /sessions/:id/message`
handler), since that also runs an agent turn.

The `_permit` is dropped at end of handler scope, releasing the slot.

If `max_concurrent_runs` is 0, the semaphore with `max(1)` still works
(minimum 1). If you want to support "unlimited", use
`tokio::sync::Semaphore::MAX_PERMITS` when value is 0:
```rust
let permits = if config.max_concurrent_runs == 0 {
    tokio::sync::Semaphore::MAX_PERMITS
} else {
    config.max_concurrent_runs
};
run_semaphore: Arc::new(tokio::sync::Semaphore::new(permits)),
```

### 4. Tests

Add one unit test or integration test verifying that with `max_concurrent_runs = 1`,
a second concurrent `/run` request blocks until the first completes. Keep it
simple — a mock or a direct semaphore test is fine.

## Acceptance

- `cargo test --workspace` green
- `cargo clippy --all-targets --all-features -- -D warnings` clean
- `cargo fmt --all` clean
- `Config` has `max_concurrent_runs` field with default 8
- `AppState` has `run_semaphore: Arc<Semaphore>`
- Both `run_handler` and `session_message_handler` acquire the semaphore before
  launching an agent turn

## Notes for the agent

- Read `src/config.rs` to understand Config struct pattern (serde defaults).
- Read `src/http/mod.rs` to see `AppState` and where it is constructed.
- Read `src/http/handlers.rs` to find `run_handler` and `session_message_handler`.
- Grep for `AppState {` to find all construction sites.
- Do NOT touch `src/agent.rs`, `src/runtime.rs`, `src/kernel.rs`, or any LLM provider.
- `tokio::sync::Semaphore` is already a dependency (tokio full features are used).
- Run `cargo test --workspace` before declaring done.
- **DO NOT call `exit_plan_mode` or `request_plan_mode`.** Running headless.
