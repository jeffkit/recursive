# Manual edit: forkSession — POST /sessions/{id}/fork + SDK wrappers

**Date**: 2026-06-02
**Goal**: Implement fork_session to create an independent copy of a session's transcript.
**Files touched**:
- `src/http.rs` — added `POST /sessions/{id}/fork` route + `fork_session` handler
- `tests/http.rs` — 2 new integration tests for fork_session
- `sdk/python/recursive_sdk/agent.py` — added `Agent.fork_session(session_id)`
- `sdk/python/tests/test_agent.py` — new file with 2 tests for fork_session
- `sdk/typescript/src/agent.ts` — added `Agent.forkSession(sessionId)`
- `sdk/typescript/tests/agent.test.ts` — 1 new test for forkSession

**Tests added**:
- `fork_session_returns_201_with_new_id` (HTTP integration)
- `fork_session_returns_404_for_missing_session` (HTTP integration)
- Python: `test_fork_session_returns_session_info`, `test_fork_session_closes_http`
- TypeScript: `Agent.forkSession calls POST...`

**Notes**:
- Fork snapshots the source transcript while holding try_lock; returns 409 if session is busy
- The forked session is fully independent: new runtime, no shared state with parent
- message_count in response reflects the transcript length at fork time
