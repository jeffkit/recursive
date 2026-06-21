# Manual edit: reduce-clone-agui-events-sse-refactor

**Date**: 2026-06-21
**Goal**: Fix three P2/P3 issues: eliminate unnecessary Vec clone in runtime, add missing AG-UI event mappings in HTTP handler, reduce cyclomatic complexity of process_sse_line in anthropic.rs.
**Files touched**:
- `src/runtime.rs` — P3-001: `emit_turn_messages` no longer clones `outcome.new_messages`; uses a borrow instead.
- `src/http/handlers.rs` — P3-003: `AguiConverter::convert` now maps `HookStarted`, `HookProgress`, `HookFinished`, `HookSystemMessage`, and `TodoUpdated` to `ag::Event::Custom` with `agui-tui/` prefixed names. Updated comment to note that `checkpoint_post` is already emitted by the driver task and `heartbeat` by the HTTP layer.
- `src/llm/anthropic.rs` — P2-004: extracted `handle_text_delta`, `handle_input_json_delta`, and `handle_thinking_delta` as private static methods on `AnthropicProvider`; `process_sse_line`'s `content_block_delta` arm now delegates to these helpers.

**Tests added**:
- `handle_text_delta_appends_content_and_skips_empty`
- `handle_input_json_delta_fills_tool_call_slot`
- `handle_thinking_delta_appends_reasoning_and_skips_empty`

**Notes**:
- `permission_request` and `file_artifact` AG-UI Custom events (g141/g140) cannot be mapped yet because the corresponding `AgentEvent` variants don't exist; a comment in the match arm documents this.
- All existing tests remain green; clippy and fmt clean.
