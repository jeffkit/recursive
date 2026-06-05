# Manual edit: user-msg-bg-and-status-gap

**Date**: 2026-06-05
**Goal**: Fix two TUI visual issues: (1) user message `>` arrow had no background, appearing detached from the highlighted text; (2) AI output had no breathing room above the status bar.
**Files touched**: src/tui/ui/transcript.rs, src/tui/ui/chat.rs
**Tests added**: none (visual-only changes)
**Notes**:
- transcript.rs: added `bg(body_bg)` to `prefix_style` so `> ` shares the same dark background as the message text.
- chat.rs: always push a blank `Line::raw("")` after all content (after spinner or after inflight lines) so there's always one empty row between the last content and the status bar.
