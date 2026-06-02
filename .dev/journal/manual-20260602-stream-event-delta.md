# Manual edit: stream_event delta — PartialAssistantMessage type

**Date**: 2026-06-02
**Goal**: Map partial_message SSE events to structured PartialAssistantMessage type,
aligned with Claude Agent SDK's SDKPartialAssistantMessage.
**Files touched**:
- `sdk/python/recursive_sdk/models.py` — added PartialAssistantMessage dataclass
- `sdk/python/recursive_sdk/__init__.py` — export PartialAssistantMessage
- `sdk/python/recursive_sdk/run.py` — yield PartialAssistantMessage for partial_message events
- `sdk/python/tests/test_run_stream_event.py` — 2 new tests
- `sdk/typescript/src/models.ts` — added PartialAssistantMessage interface
- `sdk/typescript/src/run.ts` — yield PartialAssistantMessage for partial_message events
- `sdk/typescript/src/index.ts` — export PartialAssistantMessage
- `sdk/typescript/tests/agent.test.ts` — 1 new test suite (2 tests for stream_event)

**Tests added**:
- Python: partial_message yields stream_event, result.result excludes deltas
- TypeScript: partial_message SSE events map to type="stream_event"

**Notes**:
- Backend already emits partial_message SSE events (SseEvent::PartialMessage from PartialToken)
- No backend changes required — this is a pure SDK-layer mapping
- type="stream_event" is consistent with Claude Agent SDK naming
- step field allows clients to group deltas from the same agent turn
