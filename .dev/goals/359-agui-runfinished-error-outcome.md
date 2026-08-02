# Goal 359 — AG-UI driver must report run failure, not silently emit Success

**Roadmap**: HTTP / AG-UI correctness — silent failure reporting on the SSE path

**Design principle check**:
- Implemented as: (1) a new enum variant in `agui-protocol` (protocol crate); (2) one
  branch fix in the AG-UI SSE driver in `src/http/handlers.rs`; (3) tests.
- ❌ Does NOT touch the agent kernel, run loop, tools, or any invariant. The agent runtime
  already returns `Err` correctly; the bug is purely in how the SSE driver serializes it.
- No new deps.

## Why (the silent-failure bug, with evidence)

The AG-UI SSE driver spawns the agent run and captures its `outcome`:

`src/http/handlers.rs:1709`
```rust
let outcome = runtime.run(&goal).await;
```

It records metrics correctly based on Ok/Err (`:1723-1726`):
```rust
match outcome {
    Ok(o) => record_run_success(&metrics, o.steps, &o.total_usage),
    Err(_) => record_run_failed(&metrics),
}
```

But the `RunFinished` event sent to the client **ignores `outcome` entirely** in the
non-interrupt `else` branch (`src/http/handlers.rs:1836-1844`):
```rust
} else {
    let _ = sse_tx.send(ag::Event::RunFinished(ag::RunFinished {
        thread_id: drv_thread,
        run_id: drv_run,
        outcome: Some(ag::RunFinishedOutcome::Success),  // ← always Success
        result: None,
        base: ag::BaseEvent::default(),
    }));
}
```

So when `runtime.run()` returns `Err(_)` (LLM failure, tool failure, provider down,
anything), the AG-UI client receives a `RunFinished` with `outcome: Success` and
`result: None`. The client has no way to know the run failed — it sees a "successful" run
with no result. The error is swallowed at the SSE boundary (only metrics + server logs see
it). This is silent failure reporting on the wire.

**Why the protocol can't express it today:** `RunFinishedOutcome` has only two variants
(`crates/agui-protocol/src/events.rs:62-65`):
```rust
pub enum RunFinishedOutcome {
    Success,
    Interrupt { interrupts: Vec<Interrupt> },
}
```
There is no `Error` variant. So even if the driver wanted to report the failure, it has no
outlet. The fix must add the variant to the protocol first.

## Scope (do exactly this, no more)

### 1. Add `Error` variant to `RunFinishedOutcome`

`crates/agui-protocol/src/events.rs:62` — add a third variant:
```rust
pub enum RunFinishedOutcome {
    Success,
    Interrupt { interrupts: Vec<Interrupt> },
    /// The run failed before completing. `message` carries a human-readable cause
    /// (the agent `Error` rendered via `Display`); `code` is an optional short
    /// machine-readable token (e.g. "cancelled", "rate_limited", "tool_error") the
    /// client MAY branch on. Today the driver sets only `message` from
    /// `e.to_string()`; `code` is reserved for future per-variant mapping.
    Error {
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        code: Option<String>,
    },
}
```
Serde: the enum already has `#[serde(rename_all = "camelCase", tag = "type")]`, so this
serializes as `{ "type": "error", "message": "...", "code": "..." }`. Match the existing
variant style.

### 2. Fix the driver's `else` branch to branch on `outcome`

`src/http/handlers.rs:1836-1844` — replace the unconditional `Success` with a match on the
captured `outcome`:

```rust
} else {
    let (run_outcome, result_msg) = match &outcome {
        Ok(o) => (
            ag::RunFinishedOutcome::Success,
            o.final_text.clone(),   // surface the assistant's final text if any
        ),
        Err(e) => (
            ag::RunFinishedOutcome::Error {
                message: e.to_string(),
                code: None,   // future: map Error::Cancelled etc. to codes
            },
            None,
        ),
    };
    let _ = sse_tx.send(ag::Event::RunFinished(ag::RunFinished {
        thread_id: drv_thread,
        run_id: drv_run,
        outcome: Some(run_outcome),
        result: result_msg,
        base: ag::BaseEvent::default(),
    }));
}
```
Verify `RuntimeOutcome::final_text` is the right field name (check `src/runtime.rs` /
`src/agent/types.rs` — the outcome carries the assistant's final message text). If the
field is named differently or absent, use the actual accessor; the `result` field is
`Option<String>` so `None` is acceptable when there's no final text.

The `outcome` variable is already in scope at this point (captured by the driver task
closure at `:1709`); no new capture needed. Confirm the closure captures `outcome` by
reference (it's used after the `runtime.set_event_sink(...)` at `:1712`, so it must already
be moved/captured — read the surrounding code to confirm the closure owns it).

### 3. Tests

**In `crates/agui-protocol/src/lib.rs`** (it already has round-trip tests for Success at
`:227` and Interrupt at `:247`): add `RunFinishedOutcome::Error` round-trip test mirroring
the existing ones — construct an `Error { message, code }`, serialize via `serde_json`,
deserialize, assert equality. This pins the new variant's wire format.

**In `src/http/handlers.rs`** (or `tests/http.rs` — wherever AG-UI SSE tests live): add a
test that drives the AG-UI endpoint with a provider/runtime configured to return `Err`,
and asserts the client-side `RunFinished` event has `outcome == Error { .. }` (NOT
`Success`). Look at how existing AG-UI tests build a failing runtime (likely a `MockProvider`
returning `Err(Error::Llm { ... })`). If no AG-UI SSE test currently simulates a failed
run, this is the test that locks the fix — add it. Name it
`agui_runfinished_reports_error_when_run_fails`.

## Files NOT to touch

- `src/run_core.rs`, `src/kernel.rs`, `src/runtime.rs` — the runtime already returns `Err`
  correctly; don't touch it. The bug is purely in the driver's event serialization.
- Other HTTP handlers (the non-AG-UI `run_agent` at `:130` / `send_session_message` at
  `:949` already surface errors via `ApiError::internal` — a separate issue, not this goal).
- The agent's error mapping to HTTP status codes is a separate goal; this goal only fixes
  the SSE outcome payload.
- `src/llm/`, `src/tools/`, `src/mcp.rs`, `.dev/flows/`.

## Acceptance

- `cargo test -p agui-protocol` green, including the new `Error` variant round-trip test.
- `cargo test --workspace` green, including the new AG-UI failure-outcome test.
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` clean.
- `cargo fmt --all` clean.
- Grep confirms: `rg "RunFinishedOutcome::Success" src/http/handlers.rs` no longer appears
  in the unconditional `else` branch (it's now behind the `Ok(o)` arm of the match).

## Notes for the agent (traps)

- **`tag = "type"` serde representation.** The enum uses internally-tagged JSON. The new
  `Error` variant serializes as `{"type":"error","message":"...","code":"..."}`. The
  `#[serde(default, skip_serializing_if = "Option::is_none")]` on `code` keeps it out when
  `None` — match the `Interrupt` variant's optional-field style (`message`, `tool_call_id`
  etc. all use this pattern at events.rs:75-82).
- **Backwards compatibility.** Existing AG-UI clients that only handle `Success`/`Interrupt`
  will see an unknown `type: "error"` tag. With `#[serde(tag = "type")]` + no
  `#[serde(other)]` catch-all, a strict client deserializer could reject the event. This is
  acceptable for a 0.8.x minor bump (the protocol crate is `0.1.0`, pre-1.0), and the
  alternative (silently reporting Success) is worse. Do NOT add an `#[serde(other)]` variant
  to swallow it — clients MUST learn to handle `error`. Note this in the CHANGELOG/journal.
- **`outcome` capture in the closure.** The driver is `tokio::spawn(async move { ... })` or
  similar. `outcome` is bound at `:1709` and used at `:1724` (metrics) — it's already alive
  in the same scope, so referencing it at `:1836` is fine. Don't move it twice; use `&outcome`.
- **`final_text` field name.** Verify the actual field on `RuntimeOutcome` before writing.
  It may be `final_text`, `final_message`, or the outcome may not carry text directly (you
  might need to read it from `runtime.transcript()` last assistant message). Pick the
  cleanest accessor; `None` is always acceptable for `result` if extraction is awkward.
- **Don't map `Error` variants to `code` in this goal.** That's future work (the goal text
  reserves the field). Just set `code: None` and put `e.to_string()` in `message`. A
  follow-up goal can populate `code` for `Cancelled`/`RateLimited` etc.
- **agui-protocol version.** The crate is `0.1.0` (`crates/agui-protocol/Cargo.toml`). Do
  NOT bump it in this goal unless the round-trip test requires it — the new variant is
  additive. Leave version bumps to a release-coherence goal.
