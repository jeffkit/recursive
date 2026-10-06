# Manual journal — issue #118 per-tool duration

- **Date**: 2026-10-07
- **Goal**: #118 fix(events) — the tool's execution time was computed and then
  thrown away: `ToolCallOutcome` had no duration, `AgentEvent::ToolResult` had
  no duration, so the HTTP layer reconstructed `tool_progress.elapsed_ms` from
  the `ToolCall` → `ToolResult` arrival gap. Because every result of a step is
  emitted only after the whole batch (`run_core.rs::process_tool_results`), all
  tools of a batch showed the *same* number and that number also swallowed the
  `TuiPermissionHook` approval wait. P1 gap ticket (okguitar).
- **Files touched**:
  - `src/event.rs` — `AgentEvent::ToolResult` gains `#[serde(default)]
    duration_ms: u64`: wall-clock ms measured around the tool dispatch, `0`
    for calls rejected before dispatch.
  - `src/run_core.rs` — `ToolCallOutcome.duration_ms`; the value already
    measured for the `PostToolCall` hook is now stored on the outcome (both
    the parallel `JoinSet` path and the serial path) and emitted on both
    `ToolResult` emission sites (sentinel pre-pass + main loop). Rejections
    (plan mode, plan-required, permission deny, hook skip/error, panicked
    batch task) carry `0`.
  - `src/http/handlers.rs` — drop the `tool_start_times: HashMap<_, Instant>`
    bookkeeping; the new `tool_progress_event()` forwards the event's
    `duration_ms` verbatim as `elapsed_ms`.
  - `src/http/mod.rs` — `SseEvent::ToolProgress` doc now states the new
    semantics.
  - `crates/recursive-tui/src/{model,events,backend,bash}.rs`,
    `app/event_loop.rs`, `ui/transcript.rs` — `duration_ms` flows through
    `UiEvent::ToolResult` into `ToolResultData` (as `Option<u64>`: `None` for
    blocks rebuilt from a resumed transcript, which does not persist timings),
    and is rendered next to the output size (`1.2 KB · 42ms` / `1.5s`).
  - `src/acp/bridge.rs`, `src/observability/collector.rs`,
    `crates/recursive-cli/src/cli/claude_json.rs`, `tests/http.rs` — construct
    the new field (`0`, the bridge/collector do not surface it).
  - `sdk/{python,typescript}/src…/models.*` — docstring for `elapsed_ms`
    updated to the new contract (no wire change).
- **Tests added**:
  - `src/run_core.rs`: `tool_result_events_carry_per_tool_duration` (a 50 ms
    read in the parallel batch and a 5 ms write on the serial path must report
    *different* durations — the regression guard against the batch wall
    clock), `tool_result_duration_is_zero_for_undispatched_call`.
  - `src/event.rs`: `tool_result_duration_defaults_to_zero_when_absent` (+ the
    round-trip test now asserts `"duration_ms":42`).
  - `src/http/handlers.rs`: `tool_progress_forwards_runtime_duration`,
    `tool_progress_only_for_tool_result`,
    `tool_progress_forwards_zero_for_undispatched_call` (replaces the two
    tests that simulated the removed arrival-time bookkeeping).
  - `crates/recursive-tui`: `map_tool_result_forwards_duration`,
    `render_tool_call_shows_seconds_for_slow_tool`,
    `render_tool_call_without_duration_shows_size_only`,
    `format_duration_ms_switches_at_one_second`; `tool_call_and_result_pair_by_id`
    now asserts the duration reaches the block.
- **Notes**:
  - `duration_ms` is measured around `invoke_with_audit` only, so it excludes
    both the approval wait (`permission_hook.check`) and any queueing behind
    earlier tools of the same step. The queue/approval *split* suggested in the
    ticket is left out: it needs per-call timestamps that nothing consumes yet,
    and the single honest number already answers "which tool is slow".
  - `src/observability/collector.rs` still derives its OTel tool-span duration
    from event arrival times (`tool.start` → `tool.end`, both set from
    `SystemTime::now()`), i.e. the same batch-wall-clock shape. It was left
    alone deliberately: it feeds the Langfuse/OTel export whose runtime
    visibility the ticket defers to #124. Its tests pass `duration_ms` so the
    fixture keeps compiling and is ready to be wired.
  - SSE/AG-UI consumers are unaffected: AG-UI's `ToolCallEnd` has no duration
    field, and ACP's `tool_call_update` has none either.
