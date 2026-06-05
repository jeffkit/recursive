# Manual edit: p0-security-functional-bugs

**Date**: 2026-06-06
**Goal**: Fix 6 P0-priority security and functional bugs identified in arch review (manual-20260605-arch-review.md)
**Files touched**:
- `src/permissions/auto_classifier.rs` — §8.1, §8.3
- `src/run_core.rs` — §1.2, §13.6
- `src/checkpoint.rs` — §11.4
- `src/llm/openai.rs` — §13.2

**Tests added**:
- `classifier_parse_error_defaults_block` (renamed from `classifier_parse_error_defaults_allow`, logic inverted)

**Notes**:
- **§8.1** (AutoClassifier fail-closed): `Err(_)` branch in `classify()` now returns `(true, "...block")` instead of `(false, "...allow")`. Added `tracing::warn!` for observability. Updated docstring and test.
- **§8.3** (DenialTracker fields private): Changed `pub consecutive` and `pub total` to private. External callers already use `record_denial`/`record_allow`/`is_over_limit()` — no callers broke.
- **§1.2** (Stuck detection): Replaced `consecutive_errors + last_call_key` pair with a `VecDeque<bool>` sliding window (size 10). Triggers when ≥80% of the last 10 tool calls are errors. This catches A→B→A→B cycling loops that the old identical-call check missed. Constants: `STUCK_WINDOW=10`, `STUCK_ERROR_RATE=0.8`.
- **§13.6** (DENIAL_LIMIT Invariant #8): When the denial sentinel is detected, the code now pushes tool_results for *all* calls in the batch (including the sentinel itself) before returning. Previously only N-1 results were pushed, leaving the assistant message with unpaired tool_calls and violating Invariant #8.
- **§11.4** (restore_paths path traversal): Added `crate::tools::resolve_within(&self.workspace, path)` check before `join`. Returns `Error::Tool` if path escapes workspace root (e.g. `../../../etc/passwd`).
- **§13.2** (OpenAI streaming tool_calls): `parse_sse_stream` now accumulates `delta.tool_calls[*].index/id/function.name/function.arguments` in a `HashMap<usize, (String,String,String)>` and converts to `Vec<ToolCall>` at the end. Previously `tool_calls` was always an empty `Vec::new()`, silently discarding all streaming tool calls.
