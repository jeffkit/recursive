# Manual edit: SDK Phase A — getSessionMessages + num_turns/duration_ms

**Date**: 2026-06-02
**Goal**: Complete SDK Phase A gap items: add getSessionMessages to TypeScript SDK;
confirm num_turns/duration_ms already implemented in both SDKs.
**Files touched**:
- `sdk/typescript/src/agent.ts` — added `Agent.getSessionMessages(sessionId, opts)`
- `sdk/typescript/tests/agent.test.ts` — added 2 tests for getSessionMessages

**Tests added**:
- `Agent.getSessionMessages` returns messages array from session detail
- `Agent.getSessionMessages` returns empty array when messages field is absent

**Notes**:
- Python SDK already had `Agent.get_session_messages` from a prior session
- Both Python and TypeScript SDKs already track num_turns/duration_ms in Run.stream()
- The only remaining Phase A gap was the TypeScript getSessionMessages method
