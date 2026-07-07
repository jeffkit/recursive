# Manual edit: g324 — AG-UI interrupt/resume (Pattern 2 HITL)

**Date**: 2026-07-07
**Goal**: Add interrupt-aware run lifecycle (Pattern 2) to the AG-UI
transport so runs can pause mid-turn for user input and survive a
disconnect via `resume[]` on the next `RunAgentInput`.
**Files touched**:
- `crates/agui-protocol/src/events.rs` — add `RunFinishedOutcome` enum
  (`Success` / `Interrupt { interrupts: Vec<Interrupt> }`), `Interrupt`
  struct with `id`, `reason`, `tool_call_id`, `response_schema`, etc.
  `RunFinished` now has `outcome: Option<RunFinishedOutcome>` plus legacy
  `result` for back-compat.
- `crates/agui-protocol/src/input.rs` — upgrade `Resume` from `{id, value}`
  to `{interrupt_id, status, payload}` per spec. Add `ResumeStatus` enum
  (`Resolved` / `Cancelled`, serde lowercase). Add `interrupt_before:
  Option<Vec<String>>` on `RunAgentInput` for test-only trigger.
- `crates/agui-protocol/src/lib.rs` — updated exports, added 6 new
  round-trip tests (Resume v2, RunFinishedOutcome, legacy compat,
  interrupt_before with resume).
- `crates/agui-tui/src/app.rs` — updated `resume_payload` to use
  `interrupt_id`+`status:Resolved`+`payload:{"approved":bool}`.
- `crates/agui-tui/src/main.rs` — added `interrupt_before: None` to both
  `RunAgentInput` construction sites.
- `crates/agui-client/src/lib.rs` — doc example update.
- `crates/agui-client/src/tests.rs` — added `interrupt_before: None`.
- `tests/agui_e2e.rs` — added `interrupt_before: None`.
- `src/http/handlers.rs` — major changes:
  - Added `async_trait` import.
  - Added `TestInterruptHook` (implements `PermissionHook`) — test-only
    trigger that denies tools matching `interrupt_before` and records the
    denied tool name/args for later emission as an interrupt outcome.
  - Added `OpenInterrupt` persist struct + `agui_session_dir()`,
    `load_open_interrupts()`, `save_open_interrupts()`,
    `clear_open_interrupts()` helpers. Interrupts are persisted as
    `.interrupts.json` in the AG-UI session dir so they survive crashes.
  - Added resume handling: when `input.resume` is present, loads the
    session transcript, correlates interrupts via `interruptId`, injects
    tool results from the resume payload (or sentinel for cancelled),
    seeds the runtime with `seed_transcript()`.
  - Added spec rule 4 enforcement: if a thread has open interrupts and
    no resume is provided, returns 409 Conflict.
  - Modified driver task: checks after `runtime.run()` whether a test
    interrupt was triggered, finds the denied tool result in the
    transcript, emits `StateSnapshot`/`MessagesSnapshot`/`RunFinished`
    with `Interrupt` outcome, persists the open interrupt.

**Cut taken for §4 (interrupt emit path)**:
Test-only trigger via `interrupt_before: [toolName]` + `TestInterruptHook`
that `Deny`s matching tools. After the run, the denied tool result is
found in the transcript and a `RunFinished { outcome: Interrupt { ... } }`
is emitted with the `tool_call_id` bound to the interrupt. The real
`permission_pipeline::CheckOutcome::Ask` wiring is deferred to g325.

**Tests added**:
- 6 new protocol round-trip tests in `agui-protocol` (15 total, up from 9)
- Existing `agui-tui` resume_payload test updated for new type shape
- All invariant tests pass (35/35)
- `cargo test --workspace` — green
- `cargo clippy --all-targets --all-features -- -D warnings` — clean
- `cargo fmt --all -- --check` — clean

**Verification**:
- `cargo build && cargo test --workspace` — green
- `cargo clippy --all-targets --all-features -- -D warnings` — clean
- Invariant tests (loop_size_orthogonality, sandbox, tool_call_pairing,
  test_coverage, dep_justification) — all pass
- Protocol round-trips for `RunFinishedOutcome::Interrupt` and
  `Resume { interruptId, status, payload }` verified via unit tests

**Notes**:
- Does NOT modify `src/run_core.rs::RunCore::run_inner` (invariant #1).
- Does NOT break tool-call ↔ tool-result pairing (invariant #8) — the
  interrupted tool call's `Role::Assistant(tool_calls=[id])` remains in
  the transcript, and on resume the matching `Role::Tool` is injected
  before the run continues.
- The `seed_transcript()` method on `AgentRuntimeBuilder` is used for
  the resume path to pre-populate the transcript with the modified
  session messages.
- Clippy `unwrap_used` in the hook is suppressed with per-function
  `#[allow]` and `// SAFETY: Mutex poison is unrecoverable` comments,
  matching the project's convention for poisioned-mutex recovery.
