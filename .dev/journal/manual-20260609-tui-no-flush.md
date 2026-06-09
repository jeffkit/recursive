# Manual edit: tui-no-flush

**Date**: 2026-06-09
**Goal**: Messages were disappearing after each turn because completed blocks were
being flushed into native scrollback via insert_before(). User wants all messages
to stay visible and be scrollable via Shift+Up/Down/PageUp/PageDown.

Removed the entire flush/insert_before architecture. chat.rs now renders
app.blocks (full history) so all messages stay in the ratatui viewport.
Logo (recent_display) always shows at the top.

**Files touched**: src/tui/ui/chat.rs, src/tui/mod.rs
**Tests added**: none (1171 existing tests pass)
**Notes**: The flush architecture was introduced to fix "thinking invisible" and
"scroll steps too small". Those are now non-issues: Reasoning blocks render in
the correct order already, and scroll steps were fixed separately (3/20 lines).
