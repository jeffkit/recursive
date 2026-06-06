# Manual edit: p2-planmode-goal

**Date**: 2026-06-06
**Goal**: Fix two P2 architecture issues — M-5 (hardcoded tool names) and C-2 (TOCTOU in goal budget loop)
**Files touched**:
- `src/tools/plan_mode.rs` — added `ENTER_PLAN_MODE_TOOL_NAME` and `EXIT_PLAN_MODE_TOOL_NAME` constants; updated `EnterPlanModeTool::spec()` and `ExitPlanModeTool::spec()` to use them
- `src/run_core.rs` — imported the two constants from `plan_mode`; replaced bare string literals `"exit_plan_mode"` (line ~285) and `"enter_plan_mode"` / `"exit_plan_mode"` (lines ~306-307) with the constants
- `src/runtime.rs` — merged the two separate write locks in `run_goal_loop` (increment turns + check budget exceeded) into a single write lock to eliminate the TOCTOU window (C-2); updated test assertions to use the constants via a `#[cfg(test)]` import

**Tests added**: none (existing tests for plan_mode and goal loop cover the changed paths)

**Notes**:
- M-5: The `spec()` methods now use `ENTER/EXIT_PLAN_MODE_TOOL_NAME` constants so a future tool rename becomes a compile-time error rather than a silent runtime mismatch. All three sites that compared against these strings (run_core.rs L285, L306, L307 and runtime.rs test assertions) now use the constants.
- C-2: The original code used two sequential `write()` acquisitions in `run_goal_loop`: the first incremented `turns`, released the lock, then the second checked the budget and cleared the goal. An HTTP handler calling `clear_goal()` in that window could null the goal between the two locks, causing the budget branch to be skipped or (in reverse order) GoalCleared to be emitted twice. The fix uses a single `write()` that atomically increments turns, checks the budget, sets `GoalStatus::Cleared`, and sets `goal = None` before releasing — making the check-and-clear a single critical section. No async operations cross the lock boundary, so this is safe.
- The internal `TurnOutcomeKind` enum is defined locally inside `run_goal_loop` to keep the budget decision without an additional `bool`/tuple return from the lock scope.
