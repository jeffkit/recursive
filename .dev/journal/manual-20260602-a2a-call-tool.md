# Manual edit: a2a-call-tool

**Date**: 2026-06-02
**Goal**: Implement Goal 176 — built-in `a2a_call` tool for invoking remote A2A v1.0 agents
**Files touched**:
- `src/tools/a2a.rs` (new)
- `src/tools/mod.rs` (register module + re-export + `build_standard_tools`)

**Tests added**:
- `parts_to_text_concatenates_text_parts`
- `task_state_is_terminal`
- `missing_prompt_returns_bad_tool_args_error`
- `missing_url_returns_bad_tool_args_error`
- `immediate_message_response_returns_text`
- `completed_task_response_returns_artifact_text`
- `working_task_polled_to_completion` (exercises the poll loop)
- `failed_task_returns_error_string`
- `http_error_returns_error_string`

**Notes**:
- Protocol implementation covers A2A v1.0 `POST /message:send` + polling `GET /tasks/{id}`.
- Synchronous (direct Message) and asynchronous (Task) response paths both handled.
- Mock HTTP servers built with raw `TcpListener` + `std::thread::spawn` — consistent with `web_fetch.rs` test pattern.
- `ToolSideEffect::External` — tool never runs in parallel batch.
- `timeout_secs` clamped to `[1, 300]` (fixes clippy `manual_clamp` lint).
- `authorization` parameter supports `Bearer <token>` header injection for authenticated A2A agents.
