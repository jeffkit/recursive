# Manual edit: fix-tui-viewport-flush

**Date**: 2026-06-09
**Goal**: Fix blank viewport after user sends a message. User block was marked `ready = true` in `flush_ready_blocks`, so it flushed to native scrollback immediately on submit, leaving the viewport empty (no logo, no content).
**Files touched**: src/tui/app/event_loop.rs
**Tests added**: none (no pre-existing unit tests for flush_ready_blocks; all existing tests pass)
**Notes**: User block now defers flush until the following Assistant/ToolCall block is finalized — same pattern already used for Reasoning blocks. This ensures the viewport always shows the current question + in-flight answer pair rather than going blank on Enter.
