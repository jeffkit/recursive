# Manual edit: fix-blocker-permission-and-unwrap

**Date**: 2026-06-03
**Goal**: Fix three blocker-level architecture issues found in code review
**Files touched**:
- `src/error.rs` — added `PermissionDeniedLimit` error variant
- `src/tools/mod.rs` — B1: distinguish `is_over_limit()` denial from regular denial; return new variant when over limit
- `src/run_core.rs` — B1: detect `ERROR_DENIAL_LIMIT:` sentinel in tool results and return `FinishReason::PermissionDenialLimit`; B3: replace two `unwrap()` calls in parallel tool dispatch path
**Tests added**: none (existing tests pass; integration test for denial limit requires a mock LLM)
**Notes**:
- B1 (auto-classifier deny arms identical): the `is_over_limit()` branch and the regular-deny branch previously returned the same `PermissionDenied` error, making `FinishReason::PermissionDenialLimit` permanently unreachable. Fixed by adding `Error::PermissionDeniedLimit` and propagating it via an `ERROR_DENIAL_LIMIT:` sentinel string through `execute_tool_calls` (preserves existing return type) into `run_inner`'s result loop.
- B2 (dual PermissionHook systems) was scoped out — it requires broader refactoring of `AgentRuntimeBuilder` and `TurnContext`; deferred to a separate branch.
- B3 (unwrap in JoinSet path): `join_next().unwrap()` replaced with match + tracing log; the `.find(...).unwrap()` replaced with `let Some(...) = ... else { continue }`.
