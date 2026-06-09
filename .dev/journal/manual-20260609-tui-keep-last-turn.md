# Manual edit: tui-keep-last-turn

**Date**: 2026-06-09
**Goal**: Viewport went blank after AI response completed. The previous fix deferred User block flush until response was done, but once the response finished both User and Assistant blocks flushed together, leaving the viewport empty.

New approach: flush_ready_blocks now finds the last *complete* User turn (one with no pending streaming/ToolCall blocks anywhere after it) and only flushes blocks *before* that turn. The most recent question+answer always stays in the viewport.

**Files touched**: src/tui/app/event_loop.rs
**Tests added**: none (existing 1171 tests pass)
**Notes**: Non-User blocks before the flush limit are always finalized by construction (they precede a complete User turn), so no per-block readiness check is needed in the flush loop.
