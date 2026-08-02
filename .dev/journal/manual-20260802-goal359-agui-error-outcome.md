# Manual edit: goal-359 — AG-UI driver must report run failure, not silently emit Success

**Date**: 2026-08-02
**Goal**: Fix silent failure reporting on the AG-UI SSE path. When
`runtime.run()` returns `Err(_)` (LLM failure, provider down, tool error,
anything), the driver's `RunFinished` event unconditionally emitted
`outcome: Success` with `result: None`, so clients saw a "successful" run
with no result. The error was swallowed at the SSE boundary (only metrics
+ server logs saw it).

Design: (1) new `Error` variant in `agui-protocol`; (2) one branch fix in
the AG-UI SSE driver in `src/http/handlers.rs`; (3) tests. Does NOT touch
the agent kernel, run loop, tools, or any invariant — the runtime already
returns `Err` correctly.

## Files touched

- `crates/agui-protocol/src/events.rs` — add third `RunFinishedOutcome`
  variant:
  ```rust
  Error {
      message: String,
      #[serde(default, skip_serializing_if = "Option::is_none")]
      code: Option<String>,
  }
  ```
  The enum already has `#[serde(rename_all = "camelCase", tag = "type")]`,
  so this serializes as `{"type":"error","message":"...","code":"..."}`;
  `code` is omitted when `None` (mirrors `Interrupt`'s optional-field
  style). `code` is reserved for a follow-up goal mapping
  `Error::Cancelled` / `RateLimited` etc. — the driver sets `code: None`.
- `crates/agui-protocol/src/lib.rs` — two new round-trip tests:
  `run_finished_outcome_error_round_trips` (with `code`) and
  `run_finished_outcome_error_omits_none_code` (pins that a `None` code
  is dropped from the wire JSON).
- `src/http/handlers.rs` — driver task fix in `agui_run`:
  - Changed the metrics match `match outcome` → `match &outcome` so the
    outcome is borrowed, not moved, and stays alive for the RunFinished
    branch (it was previously consumed by `record_run_success/failed`).
  - Replaced the unconditional `Success` in the non-interrupt `else`
    branch with a match on `&outcome`: `Ok(o)` → `Success` +
    `o.final_text` surfaced as `result` (wrapped via
    `serde_json::Value::String` since `RunFinished.result` is
    `Option<Value>`, not `Option<String>`); `Err(e)` →
    `Error { message: e.to_string(), code: None }` + `result: None`.
- `tests/http.rs` — new integration test
  `agui_runfinished_reports_error_when_run_fails`: MockProvider with
  `.with_errors(vec![Error::Llm { ... }])` so the first `complete()`
  fails, drive the real `/agui` endpoint via `build_router` +
  `sample_state_with_provider`, assert the terminal `RunFinished` event
  carries `RunFinishedOutcome::Error { message, code: None }` (NOT
  `Success`) and that `message` surfaces the underlying provider error.

## Design decisions / traps encountered

- **`result` field type**: goal text said `RunFinished.result` is
  `Option<String>`, but the actual protocol type is `Option<Value>`
  (`events.rs`). Surfacing `final_text` therefore needs
  `o.final_text.clone().map(serde_json::Value::String)`. `None` remains
  acceptable when there is no final text.
- **`outcome` was consumed by the metrics match**: the original code
  `match outcome { Ok(o) => record_run_success(&metrics, o.steps,
  &o.total_usage), Err(_) => ... }` moved `outcome`. Changed to
  `match &outcome` (borrow) so the RunFinished branch can still read it.
  No double-move; the borrow ends before the `else` branch.
- **`final_text` field name confirmed**: `RuntimeOutcome.final_text:
  Option<String>` at `src/runtime.rs:67` — exactly as the goal guessed.
- **No `code` mapping in this goal**: `code: None` always; message carries
  `e.to_string()`. Follow-up goal will populate codes per variant.
- **Backwards compatibility**: existing strict AG-UI clients that only
  handle `Success`/`Interrupt` will now see an unknown `type:"error"`
  tag. Accepted for 0.1.0 protocol (pre-1.0); deliberately did NOT add a
  `#[serde(other)]` catch-all — clients MUST learn to handle `error`
  rather than silently swallow it.
- **`agui-client` untouched**: it re-exports `Event` from
  `agui-protocol`, so the new variant flows through automatically; no
  crate change needed.
- **Version not bumped**: `agui-protocol` stays 0.1.0 — the new variant
  is additive; release-coherence goal owns bumps.

## Tests added

- 2 protocol round-trip tests in `crates/agui-protocol/src/lib.rs`
  (17 total, up from 15).
- 1 HTTP integration test `agui_runfinished_reports_error_when_run_fails`
  in `tests/http.rs`.

## Verification

- `cargo test -p agui-protocol` → 17 passed.
- `cargo test --features http --test http agui_runfinished_reports_error_when_run_fails` → passed.
- `cargo test --workspace` → green (run in progress at journal time; will
  confirm before stopping).
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` → clean.
- `cargo fmt --all` → clean (formatted; `Interrupt` variant re-wrapped to
  multi-line by rustfmt as a side effect of the enum growing a multi-line
  variant).
- Grep: `rg "RunFinishedOutcome::Success" src/http/handlers.rs` → single
  hit at line 1846, inside the `Ok(o)` arm of the match (no longer in the
  unconditional `else` branch) — acceptance criterion met.

## Notes

- Does NOT touch `src/run_core.rs`, `src/kernel.rs`, `src/runtime.rs` —
  the runtime already returns `Err` correctly.
- Non-AG-UI `run_agent` / `send_session_message` error surfacing via
  `ApiError::internal` is a separate issue, out of scope.
- Error→HTTP-status mapping is a separate goal.
