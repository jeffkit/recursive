# Goal 294 — Add cumulative token usage to GET /sessions/:id response

**Roadmap**: Post-Phase (API observability improvement)

**Design principle check**:
- Implemented as: add `prompt_tokens: AtomicU64` and `completion_tokens: AtomicU64`
  to `SessionState`; increment in `send_session_message` after each run;
  expose in `SessionDetailResponse`.
- ❌ Does NOT branch inside `agent.rs::Agent::run`'s main loop

## Why

`GET /sessions/:id` (SessionDetailResponse) has no cost or token usage data.
The global `/metrics` endpoint tracks total tokens across **all** sessions,
but there is no way to query "how many tokens has session X consumed?" or
"what is the estimated cost for this session?".

This is important for:
1. Client UIs showing per-session cost budgets
2. Operators doing cost-attribution per session
3. Self-improve loop tracking per-run cost

The CLI flow already tracks cost via `CostTracker` per session directory, but
the HTTP API does not expose equivalent information.

## Scope (do exactly this, no more)

### 1. `src/http/mod.rs` — add atomic counters to `SessionState`

```rust
pub struct SessionState {
    // ... existing fields ...
    /// Cumulative prompt tokens consumed in this session (all turns combined).
    pub prompt_tokens: Arc<AtomicU64>,
    /// Cumulative completion tokens generated in this session.
    pub completion_tokens: Arc<AtomicU64>,
}
```

Initialize both to 0 in `create_session` and `create_session_from_preset`.

### 2. `src/http/handlers.rs` — increment per turn in `send_session_message`

After `record_run_success(metrics, steps, &outcome.total_usage)` (which
updates global metrics), add:

```rust
let usage = &outcome.total_usage;
session
    .prompt_tokens
    .fetch_add(usage.prompt_tokens as u64, Ordering::Relaxed);
session
    .completion_tokens
    .fetch_add(usage.completion_tokens as u64, Ordering::Relaxed);
```

Look up the session from the session map (you already have it in the
handler — check if `session_arc` or the session reference is available
there, or re-lookup by session_id).

### 3. `src/http/mod.rs` / `src/http/handlers.rs` — add fields to response

Add to `SessionDetailResponse`:
```rust
/// Total prompt tokens consumed across all turns in this session.
pub prompt_tokens: u64,
/// Total completion tokens generated across all turns in this session.
pub completion_tokens: u64,
```

Populate from `session.prompt_tokens.load(Ordering::Relaxed)` and
`session.completion_tokens.load(Ordering::Relaxed)` in `get_session`.

### 4. Tests

Add a test:
- Create a session.
- Send a message (or mock the outcome).
- `GET /sessions/:id` and verify `prompt_tokens > 0` and
  `completion_tokens > 0`.

Alternatively, verify the fields exist and default to 0 for a new session.

## Acceptance

- `cargo test --workspace` green
- `cargo clippy --all-targets --all-features -- -D warnings` clean
- `cargo fmt --all` clean
- `GET /sessions/:id` response includes `prompt_tokens` and
  `completion_tokens` fields
- Both fields default to 0 for new sessions and increment after turns

## Notes for the agent

- Read `src/http/mod.rs` (SessionState struct) first.
- Read `src/http/handlers.rs` (create_session, send_session_message,
  get_session, SessionDetailResponse) to understand the full data flow.
- The usage data is in `outcome.total_usage` (a `TokenUsage` struct from
  `crate::llm`) after `execute_turn`.
- Use `Arc<AtomicU64>` so the counters can be read from `get_session`
  without holding the per-session Mutex.
- **DO NOT modify** `src/agent.rs`, `src/runtime.rs`, `src/kernel.rs`,
  `src/run_core.rs`.
