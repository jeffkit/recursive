# Manual edit: tui-mutant-debt-chat

**Date**: 2026-07-02
**Goal**: Reduce the 7 missed mutants in `crates/recursive-tui/src/ui/chat.rs`.

**Worktree**: `.worktrees/tui-mutant-debt-chat` (branch `tui-mutant-debt-chat`).

**Files touched**: `crates/recursive-tui/src/ui/chat.rs` (10 tests in `debt_tests`), `.dev/mutant-debt-20260701.md`.

**Tests added** (10, TestBackend render + buffer inspection):
- `todo_panel_height_zero_when_empty` / `_grows_with_items_and_caps_at_six`: kills 30:5 (->0).
- `render_plan_banner_on_approval_only`: kills 45:65 `||`->`&&`.
- `render_plan_mode_banner_when_pending`: kills 309:5 (-> ()).
- `render_empty_state_centers_logo_vertically`: kills 196:45 `/`->`%`.
- `render_todo_panel_visible_when_todos_present`: kills 211:5 (-> ()).
- `render_todo_panel_counts_completed_in_title`: kills 214:30 `==`->`!=`.
- `render_todo_panel_uses_content_for_pending_item`: kills 233:41 `==`->`!=`.
- `render_scrolls_up_without_panicking`: kills 117:32 `-`->`+` (overshoots total_rows -> panic).
- `render_shows_modal_when_modals_nonempty`: kills 155:8 delete `!`.

**Result**: 27 mutants → 26 caught, 1 missed. 1 unkillable: `195:20 >`→`>=` (boundary `area.height == content_h` yields zero padding either way).

**Gates**: cargo test (10 passed), clippy clean, scoped tui-mutants. Commits via `git commit-tree`.
