# Manual edit: p3-tool-session

**Date**: 2026-06-07
**Goal**: Fix two P3 issues — SendMessageTool mis-classified as ReadOnly, and list_sessions unstable pagination sort.
**Files touched**:
- `src/tools/send_message.rs`
- `src/http/handlers.rs`
- `tests/http.rs`

**Tests added**:
- `tools::send_message::tests::send_message_tool_is_not_readonly` — asserts `SendMessageTool::side_effect_class()` returns `External` and `is_readonly()` returns `false`
- `http_tests::list_sessions_stable_sort_by_id` — creates multiple sessions, fetches the list twice, and asserts the returned order is identical and sorted by `id`

**Notes**:
- `SendMessageTool` pushes messages into worker mailboxes (shared external state). It was incorrectly returning `ToolSideEffect::ReadOnly`, which would have allowed it during plan_mode when only read-only tools should run. Fixed to `ToolSideEffect::External`.
- `list_sessions` iterated over a `HashMap` whose iteration order is non-deterministic. Added `infos.sort_by(|a, b| a.id.cmp(&b.id))` before the offset/limit slice so pagination is stable across requests. `sort_by` (stable sort) is used rather than `sort_unstable_by` to preserve relative order for future equal keys.
