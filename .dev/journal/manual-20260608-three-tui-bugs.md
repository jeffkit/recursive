# Manual edit: three-tui-bugs

**Date**: 2026-06-08
**Goal**: Fix three bugs observed in TUI — thinking invisible, first reply invisible, ToolSearchTool ineffective on Anthropic provider
**Files touched**:
- `src/tui/app/event_loop.rs`
- `src/llm/anthropic.rs`

**Tests added**: Updated 2 existing tests to match new deferred-tool wire format

**Notes**:

Root cause of bug 1 & 2 was the same: `reasoning_content` is accumulated
during SSE streaming but only emitted as `AgentEvent::Reasoning` *after*
`call_llm_with_retry` returns. By that time, `PartialToken` events have
already created a `TranscriptBlock::Assistant(streaming=true)`. Pushing
`Reasoning` after it meant:
- `TurnFinished` found `Reasoning` as `blocks.last()`, not the Assistant →
  the streaming flag was never cleared → `flush_ready_blocks` stalled.
- Visual order: answer before thinking.

Fix: when `UiEvent::Reasoning` arrives and the last block is a streaming
Assistant, insert Reasoning at `len-1` (before the Assistant) instead of
pushing at the end.

Root cause of bug 3: `defer_loading: true` requires the beta header
`advanced-tool-use-2025-11-20` to be accepted by Anthropic's API. Without it
the field is silently ignored, so the model sees all tools with full schemas.

Initial fix attempt (wrong): send deferred tools with empty schema + description
prefix. Abandoned after reading fake-cc source.

Correct fix (matches fake-cc reference implementation):
- Deferred tools are NOT sent in the tools array at all.
- Their names appear in an `<available-deferred-tools>` block injected as the
  first user message, so the model knows they exist but cannot call them.
- When ToolSearchTool resolves a query, the names are stored as a JSON array
  in the marker Message content. `serialize_messages_anthropic` detects this
  and emits proper `tool_reference` blocks (required by the Anthropic beta API).
- On the next round, `extract_discovered_tool_names` scans message history for
  those markers and promotes discovered tools into the eager list with full schemas.
- The `advanced-tool-use-2025-11-20` beta header is now sent on every request.

Old functions removed: `build_request_with_partition`, `tool_reference_array`.
New functions: `build_request_with_eager_only`, `extract_discovered_tool_names`,
`inject_available_deferred`.
