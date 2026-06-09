# Manual edit: scroll-usize

**Date**: 2026-06-09
**Goal**: Trackpad/mouse scroll got stuck — couldn't scroll back down after scrolling up in long conversations. Two bugs: (1) scroll_offset was u16 (max 65535) but more importantly total_rows was also u16 and could overflow on long transcripts, corrupting max_scroll; (2) scroll_offset was never capped to max_scroll, so once it exceeded max_scroll the user couldn't scroll back to the bottom.

Fix: scroll_offset → usize, total_rows → usize (no overflow), cap scroll_offset to max_scroll before computing effective_scroll.

**Files touched**: src/tui/app/mod.rs, src/tui/ui/chat.rs
**Tests added**: none (existing tests updated automatically via type change)
**Notes**: effective_scroll passed to ratatui still needs to be u16 (API requirement), cast is safe because it's capped to max_scroll which fits in u16 via visible_rows subtraction.
