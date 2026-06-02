# Manual edit: Goal 211 — split app.rs into submodules

**Date**: 2026-06-02
**Goal**: Split `src/tui/app.rs` (3915 lines) into four focused submodules to improve
maintainability and reduce cognitive load when navigating the TUI codebase.

**Files touched**:
- `src/tui/model.rs` — new: `AppScreen`, `DiffLineKind`, `DiffLine`, `DiffHunk`, `TranscriptBlock`
- `src/tui/input_state.rs` — new: `InputMode`, `PromptInputState`, `DoublePressTracker`,
  `strip_history_prefix`, `double_press_window`, `DOUBLE_PRESS_WINDOW`, `HISTORY_CAPACITY`
- `src/tui/cost.rs` — new: `UsageStats`, `TurnState`, `default_pricing_table`,
  `estimate_cost`, `detect_model_name`
- `src/tui/completion.rs` — new: `default_offline_tool_catalog`, `search_history`,
  `glob_workspace_files`, `collect_files`, `MAX_ATFILE_SUGGESTIONS`, `MAX_HSEARCH_RESULTS`
- `src/tui/app.rs` — stripped to re-exports + `App` struct + `impl App` + helpers + tests
- `src/tui/mod.rs` — added module declarations and pub re-exports for cross-crate users

**Tests added**: none (all existing tests kept in app.rs)

**Notes**:
- All moved types re-exported from `app.rs` via `pub use` so no external import paths break
- `mod.rs` also re-exports `AppScreen`, `DiffHunk`/`DiffLine`/`DiffLineKind`, `TranscriptBlock`,
  `InputMode`, `PromptInputState`, `UsageStats` for downstream crates that import from `tui`
- `cargo test --workspace`, `cargo clippy --all-targets -- -D warnings`, and `cargo fmt --all` all
  pass cleanly after the split
- Final app.rs line count: 3252 (down from 3915); goal's ≤ 2900 estimate was slightly under
  because the test sections are comprehensive (~1560 lines of tests alone)
