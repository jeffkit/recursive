# Manual edit: tui-scroll-full-history

**Date**: 2026-06-07
**Goal**: Fix scroll truncation: when scrolling up in the TUI, messages were
cut off at the top by a 300-line RECENT_DISPLAY_MAX cap. The original inline
viewport (commit 158e35c) replaced full-history rendering with a sliding window
approach, making it impossible to scroll back to older messages.

**Root cause**: chat.rs rendered `recent_display` (capped at 300 lines) instead
of `app.blocks` (full history). The 158e35c commit introduced this regression
while adding the "natural shell history flow" feature.

**Fix**:
- `src/tui/ui/chat.rs`: render from `app.blocks` (full history) + `recent_display`
  (banner only) instead of `recent_display` + inflight blocks. Scroll now covers
  the entire transcript.
- `src/tui/mod.rs`: stop appending `print_queue` lines to `recent_display`.
  Completed blocks now go directly to `insert_before()` (scrollback) without
  accumulating in the 300-line window. Removed the now-unused `RECENT_DISPLAY_MAX`
  constant.

**Behaviour preserved**:
- Startup banner (in `recent_display`) still appears above the first message
  and scrolls with the conversation.
- `insert_before()` still pushes completed turns into the terminal's native
  scrollback, so shell history above the TUI remains continuous.
- Bottom-align padding still works (content sits flush above the status bar).
- Sticky-scroll (auto-follow new messages when at bottom) unchanged.

**Files touched**:
- `src/tui/ui/chat.rs`
- `src/tui/mod.rs`

**Tests added**: none (rendering is tested visually; all existing tests pass)
