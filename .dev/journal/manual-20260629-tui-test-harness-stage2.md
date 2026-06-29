# Manual edit: tui-test-harness stage 2 (visual acceptance tests)

**Date**: 2026-06-29
**Goal**: Wire the stage-1 harness into real *visual* acceptance tests for
the two `/resume`-area bugs fixed in `b202dc8`, proving the harness can
assert on what the user actually sees (not just internal state).

**Files touched**:
- `crates/recursive-tui/src/harness.rs` — added `Screen::bg` / `row_has_bg_color`
  / `row_has_bg_other_than` (the original `has_bg` / `row_has_bg` are too
  coarse: a panel's `Block::style(bg=Black)` fills every row, so they can't
  tell a highlight bar from the panel base); added 4 visual acceptance tests.

**Tests added** (4, in `harness::tests`):
- `theme_panel_marker_row_carries_highlight_bg` — the `▶` marker's screen row
  must carry the highlight colour `Rgb(205,100,50)`. Visual form of the
  list_offset alignment regression.
- `theme_panel_header_row_is_not_highlighted` — the "Choose theme …" header
  row must NOT carry the highlight colour. Companion guard: the buggy
  `list_offset = 0` config would highlight the header instead of the item.
- `session_resumed_replaces_visible_transcript` — pump old assistant content,
  then `UiEvent::SessionResumed` with a fresh transcript; assert the resumed
  dialogue appears and the old content vanishes from the screen (replace,
  not append).
- `session_resumed_appends_resume_note` — the "▶ Resumed session <id> (N
  messages)" note is appended after the resumed transcript.

**Effectiveness proof (manual mutation)**:
Temporarily removed `.with_list_offset(2)` from `cmd_theme` to reintroduce
the original misalignment bug. Both theme visual tests went red:
- `theme_panel_marker_row_carries_highlight_bg` FAILED (marker row lost the
  highlight — it landed on the header)
- `theme_panel_header_row_is_not_highlighted` FAILED (header row picked up
  the highlight)
Reverted the mutation → both green again. This confirms the tests bite the
real bug rather than being tautological. Stage 3 (`cargo-mutants`) automates
this kind of check across the whole touched surface.

**Design notes**:
- The highlight colour `Rgb(205,100,50)` is duplicated as a `const HIGHLIGHT`
  in the test rather than re-exported from `command_menu.rs` to avoid
  widening the production module's public API for test purposes. If the
  renderer's selected colour changes, this const must move with it (the
  mutation check above will surface drift).
- `Screen::bg` returns `None` for both `None` and `Some(Color::Reset)` so
  unset cells are treated uniformly as "no background".

**Quality gates** (in `.worktrees/feat-tui-test-harness`):
- `cargo fmt --all --check` — clean
- `cargo clippy -p recursive-tui --all-targets -- -D warnings` — clean
- `cargo test -p recursive-tui --features recursive/test-utils` — 276
  passed, 0 failed (272 from stage 1 + 4 new)

**Next**: stage 3 introduces `cargo-mutants` and codifies the
"改某文件 → 杀死该文件变异点" acceptance rule, automating the manual
mutation check done here.
