# Manual journal — issue #48 fix: `exit_plan_mode` approval wait is now cancellable (Goal 409)

- **Date**: 2026-10-03
- **Goal**: issue #48 — `recursive repl` model calls `exit_plan_mode` → turn parks
  forever (CPU 0%, Ctrl-C ineffective, only `kill -9`). Deterministic repro, no
  approver exists in the REPL, and the approval await selects on nothing.
- **Baseline note**: this branch already contains d693ff98 (#47④: 300s timeout +
  REPL wiring) and the goal-409 spec (`.dev/goals/409-plan-approval-cancellation-and-hint.md`).
  The reported "permanent park" is the pre-d693ff9 symptom; what remained broken
  at HEAD was (a) Ctrl-C only *delayed* until the 300s timeout released the gate
  (`wait_for_approval` selects on nothing), (b) wait-forever hosts (TUI/SDK,
  no timeout set) were still fully uncancellable, and (c) the REPL timeout was
  hardcoded with no waiting hint. This change closes all three.

## Files touched

- `src/tools/plan_mode.rs` — `ExitPlanModeTool` gains an optional
  `CancellationToken` (`with_cancellation_token`). The approval wait is now a
  three-way race: reviewer decision / timeout (#47④) / token (#48). The token
  path and timeout path both route through `gate.reject(...)` so `pending_plan`
  is cleared (no compaction resurrection). New `CancelReason` enum keeps the
  reason strings ASCII-only (control chars in the transcript have burned the
  OpenAI API before). Invariant #1 respected: no `run_inner` branch; the change
  lives inside the tool. Invariant #7 respected: cancellation is
  `PlanApprovalResult::Rejected` data, not an `Err`.
- `src/runtime.rs` — new `plan_approval_interrupt_token` mirror;
  `set_interrupt_token` re-registers `ExitPlanModeTool` with the token;
  `set_event_sink`/`set_approval_wait_timeout_secs`/new
  `clear_approval_wait_timeout` all funnel through one presence-guarded
  `refresh_plan_tool()` (issue #65 guard preserved: never re-introduce a
  filtered-out tool). The mirror exists because the REPL calls
  `set_event_sink` every turn *after* installing the token — without it the
  sink swap would replace the token-carrying tool with a tokenless one and
  reintroduce the bug on the second turn.
- `src/runtime/builder.rs` — initialize the new field to `None`.
- `crates/recursive-cli/src/main.rs` — REPL reads
  `RECURSIVE_PLAN_APPROVAL_TIMEOUT_SECS` (default 300; `0` = wait-forever,
  now safe because the token rescues; garbage falls back to default instead of
  failing REPL startup).
- `crates/recursive-cli/src/cli/output.rs` — `stream_events_repl` prints
  `[plan] proposed: …` plus a hint line (`waiting for plan approval — no
  reviewer is attached in repl; Ctrl-C to cancel the wait …`) so the
  "fake hang" is distinguishable from a real one (goal 409 §2).

## Tests added

- `src/tools/plan_mode.rs` (4): pre-cancelled token ends a wait-forever wait
  immediately; mid-wait cancel ends it; cancel beats the timeout in the REPL
  shape (timeout+token); an armed-but-uncancelled token does not break normal
  approvals. The first three assert `"approved":false` + cleared `pending_plan`
  (the cancel path must route through `gate.reject()`); the fourth asserts the
  reviewer decision still wins when the token is armed but never fires.
- `src/runtime/tests.rs` (2): the exact REPL order
  (`set_interrupt_token` → `set_event_sink`) preserves the token on the
  re-registered tool (dispatches the registered `exit_plan_mode` directly and
  cancels mid-wait); `set/clear_approval_wait_timeout_secs` actually refresh
  the registered tool.
- `tests/integration.rs` (1): full-turn acceptance — scripted provider calls
  `exit_plan_mode`, REPL wiring without timeout, token cancelled mid-review ⇒
  `runtime.run()` returns `FinishReason::Cancelled`, the transcript carries the
  `exit_plan_mode` tool result (`"approved":false … cancelled`), `pending_plan`
  cleared. No real gateway. This is the reported blind spot: previous coverage
  only pinned "default registry has no plan tools" (#201) and "timeout
  rejects" (#47④).
- `crates/recursive-cli/src/main.rs` (1): env matrix for
  `plan_approval_timeout_secs` in ONE test (env-race discipline).

## Verification

- `cargo test --workspace` — 3874 passed / 0 failed (58 suites).
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` — clean.
- `cargo fmt --all` — clean (`--check` green).

## Review remediation (NEEDS_FIX, 2026-10-03)

- **Bug**: in the `(Some(limit), Some(token))` arm, `wait_cancellable`
  fabricated `PlanApprovalResult::Rejected` directly in the token branch —
  bypassing `gate.reject()`, so `pending_plan` survived a REPL-shape
  (timeout+token) cancel. Consequence: after Ctrl-C mid-review, compaction's
  `PlanTodoReinjector` would re-inject "[post-compact plan restore] …" for a
  plan no longer awaiting approval (regression vs #47④, where the only exit —
  timeout — always cleared the field). `cancel_wins_over_timeout_in_repl_shape`
  missed it because it asserted only output + timing.
- **Fix**: `wait_cancellable` now returns a `WaitOutcome` marker
  (`Decision` / `Cancelled`) instead of a fabricated rejection; the caller
  routes the `Cancelled` marker through `reject_with` → `gate.reject()`, so
  every non-decision exit clears `pending_plan` exactly as the doc contract
  states. Extended `cancel_wins_over_timeout_in_repl_shape` with the
  `pending_plan().is_none()` assertion (fails against the old code).

## Notes / design decisions

- Killed the tool-arm-deletion mutant mentally (verified by temporarily
  disabling the `select!` token arm and the runtime token mirror: the new
  tests fail in both states — re-enabled afterwards).
- `CancellationToken::cancelled()` is permit-based, so a Ctrl-C landing before
  the select registers is still observed (unlike a bare `Notify::notified()`
  race); covered by the pre-cancelled test.
- `clear_approval_wait_timeout` is a new public method (0 = wait-forever is
  only meaningful as an explicit opt-out); the default stays 300s per goal 409
  ("changing the default is a product decision").
- No new dependencies (`tokio-util` was already in the tree).
