# Manual edit: fix-modal-clear

**Date**: 2026-06-04
**Goal**: Fix visual bug where dismissed modals leave stale content in scrollback
**Files touched**: `src/tui/mod.rs`
**Tests added**: none (existing test `esc_first_press_closes_modal_not_quits` already covered state correctness; this fix is purely visual)

**Notes**:
Root cause: the expand/shrink approach for modals (10→40 lines on open, 40→10 on close)
is fundamentally broken. When the viewport shrinks, ratatui creates a new smaller terminal
at the current cursor position, and the previously expanded rows (with modal content) are
pushed into the terminal's native scrollback buffer. They then appear as residue above the
TUI and scroll up with subsequent chat messages.

Multiple attempts to fix the shrink transition (ratatui Clear widget, crossterm cursor
manipulation, buffer area.y) all failed because:
1. Any content cleared before shrinking still ends up in scrollback as blank lines
2. Cursor position assumptions were incorrect due to insert_before() shifting the viewport

**Final fix**: Removed the expand/shrink logic entirely.
- Set `INLINE_HEIGHT_NORMAL = 40` (same value for all states)
- Removed `INLINE_HEIGHT_EXPANDED` constant (no longer needed)
- Removed `current_inline_height` variable (no longer needed)
- Removed the conditional viewport resize block in the main loop
- The terminal resize (on window resize events) still works correctly

The viewport is now always 40 lines: modals render in the upper portion, input/status sit
at the bottom. Completed messages still go to native scrollback via insert_before().
This matches the user's suggestion to avoid the resize-based approach entirely.
