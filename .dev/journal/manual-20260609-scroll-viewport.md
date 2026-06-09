# Manual edit: scroll-viewport

**Date**: 2026-06-09
**Goal**: Fix two persistent TUI issues: (1) thinking/reasoning content invisible, (2) unable to scroll up through output.

**Files touched**:
- `src/tui/ui/chat.rs` — viewport now renders only in-flight blocks
- `src/tui/mod.rs` — remove wrong scroll compensation, reset on flush
- `src/tui/app/commands.rs` — Shift+Up 3 lines, PageUp/Down 20 lines

**Tests added**: none (updated 4 existing scroll tests)

**Notes**:
Root cause: the viewport was rendering `app.blocks` (the entire transcript history,
growing unboundedly) rather than just the in-flight (not yet flushed) portion.
This caused two problems:

1. Thinking blocks disappeared into the middle of hundreds of lines of rendered history.
   The user was always viewing the bottom; the Reasoning block was rendered somewhere
   near the top but never visible.

2. `total_rows` grew to several hundred lines. `max_scroll` became hundreds. Pressing
   PageUp (+10) or Shift+Up (+1) barely moved the view, feeling like nothing happened.

Fix: chat.rs now renders only `blocks[last_printed_idx..]` — the live in-flight content.
Completed blocks are still pushed to native scrollback via insert_before() (unchanged),
so all history remains accessible in the terminal's own scrollback. The viewport stays
small (only the current turn's content), so scroll_offset immediately makes a visible
difference and thinking blocks are prominent.

The previous scroll-offset compensation in mod.rs was wrong and is replaced with a
simple reset-to-bottom on every flush (correct: the user just finished reading
in-flight content that is now complete, so the new bottom is the right position).

Scroll steps: Shift+Up/Down 3 lines (matches mouse wheel), PageUp/Down 20 lines.
