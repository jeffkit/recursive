# Goal 268 — Cap /agui concurrency with run_semaphore

**Roadmap**: Phase 17 (Production Hardening) — P1 from
`docs/review/architecture-review-2026-06-10.md` (NEW-HTTP-2)

**Design principle check**:
- Implemented as: acquire `state.run_semaphore.clone().acquire_owned().await`
  at the top of `agui_run`, identical to the pattern already used
  by `run_agent` and `send_session_message` (handlers.rs:78-90)
- ❌ Does NOT branch inside `agent.rs::Agent::run`'s main loop
- ❌ Does NOT add a new feature flag

## Why

`POST /agui` (handlers.rs:1207) accepts a full `RunAgentInput` and
runs the agent turn end-to-end, returning an SSE event stream.
Unlike `run_agent` (handlers.rs:78) and `send_session_message`
(handlers.rs:780), `agui_run` does **not** acquire
`state.run_semaphore`. A flood of `/agui` POSTs runs an unbounded
number of agent turns concurrently, exhausting LLM budget and
CPU. The semaphore (`state.run_semaphore`, configured via
`max_concurrent_runs` in the operator config) is the
load-shedding primitive; the `/agui` endpoint bypasses it.

This is a 1-line fix with a 5-line test. The pattern is already
in the codebase twice — copy it.

## Scope (do exactly this, no more)

### 1. `src/http/handlers.rs` — add semaphore acquire to `agui_run`

In the `agui_run` function (line 1207), immediately after parsing
the input (i.e. after the `serde_json::from_value` block ends),
add:

```rust
// Bound concurrent agent turns via the shared run_semaphore.
// Without this, an unbounded number of /agui requests would run
// in parallel and exhaust the LLM budget. Pattern matches
// run_agent (line 78) and send_session_message (line 780).
let _permit = state
    .run_semaphore
    .clone()
    .acquire_owned()
    .await
    .map_err(|e| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ErrorResponse {
                status: "error".into(),
                error: format!("run semaphore closed: {e}"),
            }),
        )
    })?;
```

Note: `_permit` must be bound to a name (not `let _ = ...`) so
the RAII guard is held for the full function body. Drop happens
on function return.

If `agui_run` is structured to call an inner async function (e.g.
`process_agui_run(state, input, permit)`), the `permit` must be
moved into the inner call. Check the existing structure; the
`_permit` binding only needs to live until the inner call
returns, at which point the inner function has finished consuming
the budget.

### 2. Tests in `src/http/handlers.rs` `#[cfg(test)] mod tests`

Add a test that verifies `/agui` honours the run_semaphore
backpressure:

- **test_agui_respects_run_semaphore** — Spawn a `run_agent` in
  the background (which holds the semaphore). Issue a `POST /agui`
  request. Assert it returns 503 (or the chosen status) within a
  short timeout (e.g. 1s) instead of running unbounded.

  If integration test plumbing for handlers is non-trivial
  (likely — handlers depend on `AppState` which depends on the
  full LLM/runtime), write the test at the unit level on
  `run_semaphore` only and assert that `agui_run`'s body **calls**
  `acquire_owned()`. A code-walk assertion is acceptable: use
  `grep "acquire_owned" src/http/handlers.rs` in the test, OR
  parse the file with `syn` (if added), OR just open the file in
  the test and assert the string `"acquire_owned"` appears in
  `agui_run`. The simplest acceptable form: a snapshot test that
  greps the source.

  The full integration test (acquire semaphore in fixture, fire
  /agui, expect 503) is a stretch goal; skip if the existing test
  harness makes it too costly.

### 3. No changes outside `src/http/handlers.rs`

Do not touch any other file. This is a single-file, single-function
fix. The semaphore is already wired into `AppState`; we just need
to call it.

## Acceptance

- `cargo test --workspace` — green (existing + new test pass)
- `cargo clippy --all-targets --all-features -- -D warnings` —
  clean
- `cargo fmt --all` — applied
- `grep "acquire_owned" src/http/handlers.rs` — should now show
  3 matches (run_agent, send_session_message, agui_run) instead
  of 2
- A new test in the file's `#[cfg(test)] mod tests` exercises
  either the source-grep snapshot form or the full integration
  form (either is acceptable).

## Notes for the agent

- The status code `SERVICE_UNAVAILABLE` (503) is a reasonable
  default for "the server is at capacity". If the existing
  `run_agent` returns a different code on semaphore exhaustion
  (e.g. `TOO_MANY_REQUESTS` 429), match that for consistency —
  read lines 78-90 first.
- The semaphore in `AppState` is `Arc<tokio::sync::Semaphore>`
  (verify in `http/mod.rs`). `acquire_owned()` returns
  `Result<OwnedSemaphorePermit, AcquireError>`. On error the
  semaphore is closed (rare — only on shutdown), so 503 is fine.
- Do NOT use `acquire()` (borrowing) — that holds the semaphore
  permit tied to the `&state` borrow, which prevents the
  function body from moving state into spawned tasks. The
  `_owned` form is mandatory.
- DO NOT introduce a new error variant — reuse the existing
  `ErrorResponse` struct and tuple-shaped return type.
- Estimated diff: 1 file, +15 lines (semaphore block + 1
  #[ignore]'d integration test or 1 source-grep test).

**Out of scope**: rate limiting per IP, auth-bypass for /agui,
SSE timeout. These are separate P1/P2 items in the review.

**Status: TODO**. Lead will trigger this goal in the next
self-improve run after A is fully closed.
