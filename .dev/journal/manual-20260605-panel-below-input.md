# Manual edit: panel-below-input

**Date**: 2026-06-05
**Goal**: Move slash-command / @file / history-search popups from floating
overlays above the input box to a dedicated panel slot *below* the input.
When the panel is active the input box moves up naturally (the messages
area shrinks to make room); when dismissed the slot collapses to 0 and
everything returns to the normal position.

**Approach**: Layout slot (Option A)
- Add `Constraint::Length(panel_h)` as the last constraint in the 6-slot
  `Layout::Vertical` in `chat.rs`.  `panel_h = command_menu::panel_height(app)`
  returns 0 when no interactive mode is active.
- Add `panel_height(app) -> u16` and `render_panel(frame, area, app)` to
  `command_menu.rs`; the latter dispatches to private helper functions
  `render_command_panel / render_atfile_panel / render_history_panel` that
  render into the Layout-provided `Rect` (no `popup_rect` computation, no
  `Clear` needed since the slot is owned by the Layout).
- Remove the three old overlay calls (`render`, `render_atfile`,
  `render_history_search` with `chunks[4]` as input_area) from `chat.rs`.
  `render_permission_modal` remains as a centred overlay (it needs to be
  prominent and doesn't fit the panel model).

**Files touched**:
- `src/tui/ui/command_menu.rs` — added `panel_height`, `render_panel`,
  and three private panel helpers
- `src/tui/ui/chat.rs` — updated Layout (5→6 constraints), removed
  overlay calls, added `render_panel` at `chunks[5]`

**Tests added**: none (purely visual / layout change; existing command_menu
tests still pass)

**Notes**:
- Option B (dynamic viewport height expansion) was evaluated but is
  incompatible with the current full-height viewport: the viewport already
  = terminal height so there is no room to expand without complex ghost
  prevention.  Option A achieves the same visual effect (input moves up,
  panel appears below) with zero ghost risk.
