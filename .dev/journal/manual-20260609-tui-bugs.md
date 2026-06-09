# Manual edit: tui-bugs

**Date**: 2026-06-09
**Goal**: Fix three TUI UX issues reported by user: (1) thinking/reasoning content not visible, (2) scroll becomes locked after large Bash output, (3) related infrastructure for correct scroll position after flush.

**Files touched**:
- `src/tui/app/event_loop.rs` — two fixes
- `src/tui/mod.rs` — one fix

**Tests added**: none (existing tests cover the changed code paths)

**Notes**:

### Fix 1: Reasoning (thinking) blocks disappear immediately

`flush_ready_blocks` had a Reasoning defer rule that only waited for a following
*streaming* Assistant block. If Reasoning was followed by a ToolCall (still running)
or was the last block, it flushed immediately into scrollback and vanished from the
viewport before the user could read it.

New rule: defer Reasoning flush until the next block is fully finalized (non-streaming
Assistant, ToolCall with result, or any other finalized block). If there is no next
block yet (Reasoning just arrived), keep it in the viewport too. This guarantees
thinking content is always visible while the agent is working.

### Fix 2: Scroll locks up after large Bash output

`flush_ready_blocks` returned `()`. When it flushed rows to the scrollback via
`print_queue`, `scroll_offset` stayed at whatever value the user had set while
scrolling up. After the flush, `max_scroll` (computed from the remaining live
viewport content) became smaller, so `scroll_offset.min(max_scroll)` was clamped
to a low value and `effective_scroll` dropped near 0 — the viewport snapped to
the bottom and both up and down scrolling appeared to do nothing.

Fix: `flush_ready_blocks` now counts and returns the number of visual rows queued.
The main loop in `mod.rs` adds that count to `scroll_offset` when the user is
scrolled up (scroll_offset > 0), compensating for content that left the viewport
into native scrollback. The user's reading position stays stable.
