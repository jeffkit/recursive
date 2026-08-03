# Manual landing — TUI input box auto-wrap for long pastes

- date:   2026-07-31
- goal:    fix pasted long text not wrapping / getting clipped in the prompt input box
- mode:    manual edit (outside the self-improve flow)
- branch:  tui-input-autowrap
- worktree: `.worktrees/input-autowrap`
- verdict: completed — all quality gates green

## Problem

Pasting a long paragraph into the TUI input box left it stuck at one row:
the tail was invisible and the cursor sat at a clamped position, so the
user couldn't see — let alone keep typing at — the end of the paste.

## Root cause

Two coupled bugs, both in `crates/recursive-tui/src/ui/input.rs`:

1. `total_height(app)` estimated the box height from **logical** lines
   (`buffer.lines().count()`). A long paste with no `'\n'` is one logical
   line, so the layout reserved only one content row for it.
2. `render` built the wrapped `Paragraph` (the soft-wrap logic already
   existed) but rendered it **top-anchored with no scroll offset**, then
   clamped the cursor to the visible area. Anything past the first
   `visible_rows` wrapped rows was clipped, and the real edit position
   was off-screen.

## What landed

**`crates/recursive-tui/src/ui/input.rs`**

- `total_height` now takes `area_width` and folds every logical line
  through the existing `wrap_line_by_width`, summing the resulting visual
  rows (clamped to `MAX_VISIBLE_ROWS`). Extracted `BORDER_WIDTH` /
  `PREFIX_WIDTH` constants and a shared `available_text_width_from` so the
  estimator and the renderer agree on the available column count.
- `render` computes the cursor's wrapped row first, derives a
  `scroll_y = cursor_row.saturating_sub(visible_rows - 1)` (editor-style
  follow), passes it to `Paragraph::scroll((scroll_y, 0))`, and subtracts
  `scroll_y` when translating the cursor back to screen coordinates. The
  cursor's row now always lands inside the visible window, so the tail of
  a long paste stays editable.

**`crates/recursive-tui/src/ui/chat.rs`**

- Single call site updated to `input::total_height(app, frame.area().width)`.

## Tests added

- `total_height_grows_with_wrapped_long_line` — a long single-line buffer
  expands the box past one row and caps at `MAX_VISIBLE_ROWS`.
- `render_scrolls_to_keep_cursor_visible_when_buffer_overflows` — a
  31-char buffer at width 8 (8 wrapped rows, 3 visible) with the cursor at
  the end must paint the sentinel tail on screen.
- Existing `total_height_grows_with_lines_until_cap` updated for the new
  signature.

## Quality gates (all green)

- `cargo test --workspace` — 0 failed across all crates.
- `cargo clippy -p recursive-tui --all-targets -- -D warnings` — clean.
- `cargo fmt --all --check` — clean.
- `.dev/scripts/tui-test-presence.sh` — PASS (test-bearing change
  detected in `crates/recursive-tui/src/ui/input.rs`).

## Notes

- No kernel / run-loop changes — `input.rs` + `chat.rs` only, so none of
  the 8 source invariants are touched.
- `tui-mutants` is advisory for manual edits per the root AGENTS.md; not
  run here. The Flowcast self-improve flow still enforces it as a hard
  gate for its own runs.
