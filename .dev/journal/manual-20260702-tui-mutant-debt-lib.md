# Manual edit: tui-mutant-debt-lib

**Date**: 2026-07-02
**Goal**: Reduce the 5 missed mutants in `crates/recursive-tui/src/lib.rs`.

**Worktree**: `.worktrees/tui-mutant-debt-lib` (branch `tui-mutant-debt-lib`).

**Files touched**: `crates/recursive-tui/src/lib.rs` (3 tests in `tests`), `.dev/mutant-debt-20260701.md`.

**Tests added** (3):
- `handle_mouse_scroll_up_increases_offset`: kills 215:9 (delete ScrollUp).
- `handle_mouse_scroll_down_decreases_offset`: kills 218:9 (delete ScrollDown).
- `install_tui_panic_hook_writes_log_when_quiet`: kills 87:5 (install_tui_panic_hook -> ()) and 90:16 (delete `!` in the quiet guard) — asserts the panic marker lands in `tui-panic.log` when `is_tui_quiet` is true.

**Result**: 9 mutants → 8 caught, 1 missed. 1 unkillable: `60:9 RawModeGuard::drop -> ()` (terminal side effects not observable from a unit test).

**Gates**: cargo test (3 passed), clippy clean, scoped tui-mutants. Commits via `git commit-tree`.
