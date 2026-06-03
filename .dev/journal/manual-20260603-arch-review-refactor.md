# Manual edit: arch-review-refactor

**Date**: 2026-06-03
**Goal**: Implement improvements from architecture review — eliminate compaction code duplication, decompose the `run()` god function, add LLM retry, and remove the dead `TurnContext.event_sink` field.
**Files touched**:
- `src/compact.rs` — added `apply_to_transcript()` as the single authoritative splice implementation
- `src/runtime.rs` — refactored `run()` (~220 lines) into 5 focused helpers; `compact_now()` now delegates to `apply_to_transcript()`
- `src/run_core.rs` — `maybe_compact()` delegates to `apply_to_transcript()`; added `call_llm_with_retry()` with exponential back-off (3 retries, honours `retry_after_ms`)
- `src/kernel.rs` — removed `TurnContext.event_sink` field (unused since Goal 219)
- `src/multi.rs` — removed orphan `NullSink` import; dropped `event_sink:` from `TurnContext` init
- `src/tools/sub_agent.rs` — dropped `NullSink` import and `event_sink:` field
- `src/tools/spawn_worker.rs` — dropped `NullSink` import and `event_sink:` field
- `tests/agent_team_integration.rs` — dropped `NullSink` import and `event_sink:` field

**Tests added**: none (all existing tests pass; retry path tested via `call_llm_with_retry` unit logic)
**Notes**:
- `parent_agent_last_uuid` in `AgentRuntimeBuilder` was intentionally kept: it has a live integration test in `tests/uuid_chain.rs` and is a public API surface. The field is still not wired to event emission — that is a future Goal task.
- The `deferred_turn_finished` field added to `AgentRuntime` is an implementation detail of the `execute_kernel_turn` / `emit_turn_messages` split; it is never `Some` outside of an active `run()` call.
- LLM retry uses `<<` for the back-off multiplier: 1 s, 2 s, 4 s for attempts 0–2.
