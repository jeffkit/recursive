# Manual edit: fix-modal-clear

**Date**: 2026-06-04
**Goal**: Fix visual bug where dismissed modals leave stale content on screen after ESC key press
**Files touched**: `src/tui/mod.rs`
**Tests added**: none (existing test `esc_first_press_closes_modal_not_quits` already covered state correctness; this fix is purely visual)

**Notes**:
The modal state was correctly managed (ESC correctly pops the modal from `app.modals`),
but a visual artifact remained on screen. When the modal was closed, the viewport shrank
from INLINE_HEIGHT_EXPANDED (40) back to INLINE_HEIGHT_NORMAL (10). The new 10-line
terminal rendered at the bottom, but the upper 30 lines (where the modal had been displayed)
were not cleared — leaving stale modal content visible.

Fix: before creating the smaller terminal instance, draw a blank frame (`ratatui::widgets::Clear`)
using the still-expanded terminal. This overwrites the old modal content with spaces, so when
the viewport shrinks, there is no stale content visible above it.
