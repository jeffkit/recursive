# Manual edit: SDK Phase B — tool_progress events

**Date**: 2026-06-02
**Goal**: Implement SDK Phase B: expose real-time tool execution timing as `tool_progress` SSE events, and map them in both the Python and TypeScript SDKs.

## What was done

### Backend (`src/http.rs`)
- Added `SseEvent::ToolProgress { tool_use_id, tool_name, elapsed_ms }` variant
- Updated `session_events` SSE stream match arm to emit `"tool_progress"` event type
- Made the session message forwarder stateful: maintains a `HashMap<String, Instant>` keyed on tool_use_id; on `AgentEvent::ToolCall`, records start time; on `AgentEvent::ToolResult`, computes elapsed and broadcasts `ToolProgress` after the `ToolResult`
- Added 2 unit tests: `tool_progress_emitted_after_tool_result` and `tool_progress_elapsed_is_zero_for_unmatched_result`

### Python SDK (`sdk/python/`)
- `models.py`: added `ToolProgressMessage(type, tool_use_id, tool_name, elapsed_ms, session_id)` dataclass
- `run.py`: import `ToolProgressMessage`; handle `ev_type == "tool_progress"` in `messages()` generator
- `__init__.py`: export `ToolProgressMessage`

### TypeScript SDK (`sdk/typescript/`)
- `models.ts`: added `ToolProgressMessage` interface, added to `SDKMessage` union type
- `run.ts`: import `ToolProgressMessage`; handle `evType === "tool_progress"` in `stream()` generator
- `index.ts`: export `ToolProgressMessage`
- Rebuilt dist (CJS + ESM + DTS)

**Files touched**:
- `src/http.rs`
- `sdk/python/recursive_sdk/models.py`
- `sdk/python/recursive_sdk/run.py`
- `sdk/python/recursive_sdk/__init__.py`
- `sdk/typescript/src/models.ts`
- `sdk/typescript/src/run.ts`
- `sdk/typescript/src/index.ts`
- `sdk/typescript/dist/` (rebuilt)

**Tests added**:
- `src/http.rs::tests::tool_progress_emitted_after_tool_result`
- `src/http.rs::tests::tool_progress_elapsed_is_zero_for_unmatched_result`

**Notes**:
- The agent core (`agent.rs`, `event.rs`, `runtime.rs`) is untouched — timing is computed in the HTTP layer only. This keeps the implementation lightweight and prevents any risk to the agent loop.
- `elapsed_ms` is wall-clock time in the HTTP forwarder, so it includes network/queue time between events. It is accurate enough for SDK consumers to show "took Xs" in UI.
- Both SDKs yield `ToolProgressMessage` from `.stream()` / `.messages()`, discoverable by consumers filtering on `msg.type === "tool_progress"` (TS) or `isinstance(msg, ToolProgressMessage)` (Python).
