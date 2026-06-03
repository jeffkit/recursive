# Manual edit: split-tui-app

**Date**: 2026-06-03
**Goal**: Split the 3303-line monolithic `src/tui/app.rs` into 5 focused files under `src/tui/app/`
**Files touched**:
- `src/tui/app/mod.rs` — App struct + PendingPermission + re-exports (pre-existing, 146 lines)
- `src/tui/app/state.rs` — constructors, accessors, transcript helpers, set_pending_permission (255 lines)
- `src/tui/app/event_loop.rs` — handle_ui_event + streaming helpers + start_turn (651 lines)
- `src/tui/app/commands.rs` — all keyboard handlers + modal dispatch + atfile/history/permission tests (2163 lines)
- `src/tui/app/render.rs` — preview_args, verb_for_tool, parse_v4a_patch, extract_write_file_path_from_result (195 lines)
- Deleted: `src/tui/app.rs`
**Tests added**: Distributed all original test modules across new files:
  - state.rs: construction + pricing tests
  - event_loop.rs: streaming, tool call, plan events, sticky-scroll tests
  - commands.rs: key handling, prompt_input_tests, atfile_tests, hsearch_tests, perm_tests
  - render.rs: preview_args, verb_for_tool, parse_v4a_patch tests
**Notes**:
- `extract_write_file_path_from_result` made `pub(crate)` so event_loop.rs can call it
- `start_turn` and `toggle_last_expandable` made `pub(crate)` so commands.rs can call them
- All three quality gates pass: `cargo test --workspace`, `cargo clippy --all-targets --all-features -- -D warnings`, `cargo build`
- commands.rs is ~2163 lines (over the 1000-line target) due to the 5 large test modules moved there
