# Manual edit: fix-plan-mode-request-hang

**Date**: 2026-06-20
**Goal**: Fix `request_plan_mode` tool hanging forever without prompting the user for confirmation.

## Root cause

When the agent calls `request_plan_mode`, `RequestPlanModeTool::execute` emits a
`PlanModeRequested` event and then blocks on `PlanModeRequestGate::wait_for_decision()`.

Meanwhile, `worker_loop` handles the running turn inside `run_turn_select_loop`. That
select loop already handles `ConfirmPlan`/`RejectPlan` by forwarding them to the
`PlanApprovalGate`, but `ApprovePlanMode` / `RejectPlanMode` fell through to the
catch-all `Some(_) => {}` arm and were **silently discarded**.

The backend's top-level `ApprovePlanMode`/`RejectPlanMode` match arms tried to call
`rt.approve_plan_mode_request()` on `rt_opt.as_ref()`, but `rt_opt` is `None` during
a running turn (the runtime was taken via `rt_opt.take()` and moved into the spawned
task). The warning "backend: runtime not available in ApprovePlanMode" was logged and
the gate was never woken, causing the tool to block indefinitely.

## Fix

**`src/runtime.rs`**: Added `plan_mode_request_gate() -> Arc<PlanModeRequestGate>`
public accessor method (mirrors the existing `plan_approval_gate()` method).

**`src/tui/backend.rs`**:
- Added `plan_mode_request_gate: &Arc<PlanModeRequestGate>` parameter to
  `run_turn_select_loop`.
- Added `#[allow(clippy::too_many_arguments)]` since the function now takes 8 args.
- Handled `UserAction::ApprovePlanMode` and `UserAction::RejectPlanMode` in the select
  loop, forwarding them directly to `plan_mode_request_gate.approve()` /
  `plan_mode_request_gate.reject(&reason)`.
- Updated all three callers (`SendMessage`, `ConfirmPlan`, `SetGoal`) to extract
  `rt.plan_mode_request_gate()` before taking the runtime and pass it to the loop.

**Files touched**:
- `src/runtime.rs`
- `src/tui/backend.rs`

**Tests added**: none (existing tests all pass; the bug was a runtime-concurrency
interaction not easily covered by unit tests without adding async integration test
infra).

**Notes**: The second symptom reported — "sometimes banner appears but y/Enter has no
response" — is explained by the same root cause: the `ApprovePlanMode` action was
emitted by the TUI key handler but discarded before reaching the gate.

---

## Fix 2: inline Plan Proposal y/n/e not intercepted

**Date**: 2026-06-20

**Root cause**: Fix-E changed `PlanProposed` display from a floating modal to an inline
`TranscriptBlock::PlanProposal`. The existing key handler for plan review
(`handle_plan_review_key`) is only reached via `handle_modal_key_action`, which only
fires when `!self.modals.is_empty()`. With no modal on the stack, y/n/e fall through to
the prompt input buffer and the user's keystrokes are typed into the chat box instead of
approving/rejecting the plan.

**Fix** (`src/tui/app/commands.rs`):
- Added `handle_inline_plan_review_key` — handles y/Enter (ConfirmPlan), n/Esc
  (RejectPlan "user rejected"), e (copy plan text to buffer + RejectPlan "user edited"
  so the gate unblocks), and any other key is silently consumed to keep focus.
- Added a `plan_awaiting_approval` guard at the top of `handle_key`, before the modal
  stack check, that routes to `handle_inline_plan_review_key`.
- Added 5 unit tests covering all key paths.

**Files touched**: `src/tui/app/commands.rs`
