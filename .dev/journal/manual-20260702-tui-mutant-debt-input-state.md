# Manual edit: tui-mutant-debt-input-state

**Date**: 2026-07-02
**Goal**: Reduce the 11 missed mutants in `crates/recursive-tui/src/input_state.rs`.

**Worktree**: `.worktrees/tui-mutant-debt-is` (branch `tui-mutant-debt-is`).

**Files touched**: `crates/recursive-tui/src/input_state.rs` (6 tests in `tests`), `.dev/mutant-debt-20260701.md`.

**Tests added** (6):
- `delete_forward_removes_char_at_cursor`: kills 202:9 (-> ()), 202:24 (`>=`->`<`), 208:39 (`+`->`-`).
- `move_end_lands_at_line_newline`: kills 251:34 `+`->`*`/`-`.
- `move_next_line_advances_past_trailing_newline`: kills 302:28 `>`->`==`/`>=`.
- `cursor_on_last_line_true_when_no_trailing_newline`: kills 320:9 (-> false).
- `history_prev_stashes_current_buffer_as_draft`: kills 326:9 (enter_history_walk -> ()).
- `record_submission_drains_overflow_beyond_capacity`: kills 384:51 `-`->`/`.

**Result**: 85 mutants → 81 caught, 1 missed, 3 unviable. 1 unkillable: `383:35 >`→`>=` (push makes len ≥ cap+1, so `>` and `>=` are equivalent).

**Gates**: cargo test (16 passed), clippy clean, scoped tui-mutants. Commits via `git commit-tree` (no Co-authored-by).
