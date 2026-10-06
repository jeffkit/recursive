# Manual change — issue #119

- **Date:** 2026-10-06
- **Goal:** #119 fix(multi): worker 的事件与 usage 被 NullSink 吞掉——父会话里 worker
  是黑盒且成本系统性低估
- **Files touched:**
  - `src/event.rs` — new `AgentEvent::WorkerEvent { worker_id, task_id, event }`
    variant + `WorkerEventSink` (attributes a worker's events and forwards them
    to a parent sink **held weakly**, wrapped so transcript persistence ignores
    them).
  - `src/tools/agent.rs` — `WorkerUsage` / `WorkerTelemetry` / `WorkerTelemetrySlot`
    (the published sink is a `Weak`, see the blocker note below);
    `AgentTool::with_worker_telemetry`; `build_worker_runtime` attaches the
    parent sink via `WorkerEventSink`; `run_worker` and the background worker
    loop record `total_usage` + `llm_latency_ms` into the bridge.
  - `src/tools/mod.rs` — re-exports.
  - `src/multi.rs` — `register_subagent_if_enabled` takes the telemetry slot.
  - `src/runtime.rs` / `src/runtime/builder.rs` — runtime holds the bridge,
    publishes its sink into it (`build` + `set_event_sink` /
    `replace_event_sink`), drains worker usage into the turn **and** into
    `last_failed_usage` on the error path.
  - `src/observability/collector.rs` — a worker `Usage` counts toward the trace
    totals (never the parent's step list).
  - `src/lib.rs` — re-export `WorkerEventSink`.
  - Call sites: `crates/recursive-cli/src/cli/builder.rs`,
    `crates/recursive-cli/src/main.rs` (loop wires it; HTTP passes `None`),
    `crates/recursive-cli/src/cli/output.rs` — `format_worker_event` renders a
    `[worker <id>] …` line in the human streams,
    `crates/recursive-cli/src/cli/claude_json.rs` — explicit, documented drop,
    `crates/recursive-tui/src/runtime_builder.rs` (both build paths),
    `crates/recursive-tui/src/events.rs` — new `UiEvent::WorkerUsage`,
    `crates/recursive-tui/src/backend.rs` — `map_agent_event` maps a worker's
    `Usage` onto `UiEvent::WorkerUsage`,
    `crates/recursive-tui/src/cost.rs` — `UsageStats::record_worker_usage`
    (session totals only),
    `crates/recursive-tui/src/app/event_loop.rs` — applies worker usage to the
    totals,
    `tests/issue407-cancel-surfaces.rs`.
- **Tests added:** `event::tests::worker_event_*` (+ round-trip in
  `additional_event_variants_round_trip`),
  `event::tests::worker_event_sink_releases_the_parent_sink`,
  `tools::agent::tests::worker_telemetry_*`
  and `worker_without_telemetry_uses_the_null_sink`,
  `runtime::tests::worker_telemetry_usage_is_folded_into_the_turn`,
  `runtime::tests::set_event_sink_republishes_to_the_worker_bridge`,
  `runtime::tests::worker_bridge_does_not_pin_the_parent_sink`,
  `runtime::tests::failed_turn_bills_worker_usage_to_last_failed_usage`,
  `recursive_tui::app::event_loop::tests::worker_usage_updates_totals_only`,
  `observability::collector::tests::worker_usage_counts_toward_trace_totals_but_not_steps`,
  `observability::collector::tests::worker_non_usage_events_do_not_touch_parent_trace`,
  `recursive_tui::backend::tests::map_worker_usage_reaches_the_usage_panel`,
  `recursive_tui::backend::tests::map_worker_non_usage_event_is_dropped`,
  `recursive_tui::backend::tests::tui_event_sink_emit_forwards_worker_usage`,
  `recursive_cli::cli::output::tests::format_worker_event_*`.
- **Notes:**
  - Worker events are **wrapped** (`WorkerEvent`) rather than forwarded raw:
    forwarding a worker's `MessageAppended` un-wrapped would let
    `SessionPersistenceSink` write worker messages into the **parent** session,
    and a raw worker `TurnFinished` would advance the parent's Langfuse turn.
  - The worker's `step` numbers are its own; the collector therefore folds only
    worker `Usage` into the trace totals.
  - HTTP server passes `None`: it shares one `Agent` tool across concurrent
    sessions, so a single shared sink slot would leak one session's worker
    events into another's SSE stream. Per-session worker telemetry for HTTP
    needs a per-session `Agent` tool (out of scope here); workers keep the
    previous `NullSink` behaviour on that channel. **Known residual gap:** the
    HTTP run-metrics / SSE surfaces therefore still exclude worker spend and
    activity; the fix is applied on the CLI run / loop / resume paths, the TUI
    usage panel, and the Langfuse trace totals.
  - Live consumers of the forwarded events: the run collector folds a worker
    `Usage` into the trace totals; the TUI maps it onto the session usage
    panel; the CLI human streams print a `[worker <id>] …` line. The Claude
    wire drops worker events (its usage already lands in the terminal `result`
    via the runtime outcome).
  - **Sink lifetime (review blocker, fix round 2).** A `background: true`
    worker's runtime parks on its prompt channel and outlives the run that
    spawned it, so anything it holds strongly outlives the run too. Both
    paths from a worker to the parent sink are now **weak**:
    `WorkerEventSink::inner: Weak<dyn EventSink>` (drops the event when the
    parent is gone) and `WorkerTelemetry::event_sink: Option<Weak<dyn EventSink>>`.
    With a strong reference the CLI's run-scoped `ChannelSink` sender stayed
    alive for the whole process, and `recursive run` hung at
    `handle.await` / the JSON printers' `while rx.recv().await.is_some()`, while
    `recursive repl` never returned to its prompt. The runtime's own
    `event_sink` field is the one strong owner, so the channel now closes when
    the runtime is dropped / the REPL swaps back to `NullSink`. Two round-1
    tests (`worker_telemetry_event_sink_round_trips`,
    `worker_telemetry_records_usage_and_forwards_events`) published a temporary
    `Arc::new(sink)` and so now held the only strong reference; they were
    updated to keep the owning handle alive, which is what the real callers
    do. The regression test
    `tools::agent::tests::background_worker_does_not_pin_the_parent_sink`
    reproduces the CLI printer loop around a live `background: true` worker; it
    was verified to fail (5 s timeout) against a temporarily-strong bridge and
    pass with the weak one. Consequence for the REPL: a worker spawned in turn
    N keeps the turn-N sink (now dead), so its later *events* are dropped
    rather than bleeding into turn N+1's stream — same as the pre-#119
    `NullSink` behaviour for that case — while its *usage* still reaches the
    bridge and is billed, because usage is recorded through the slot, not the
    sink.
  - **Failed-turn worker spend (review secondary).** `drive_turn_inner` drains
    the bridge only on the `Ok` path, so `execute_kernel_turn`'s `Err` branch
    drains it into `last_failed_usage` itself; otherwise a failed turn's worker
    spend was reported as zero by the CLI cost tracker and silently re-billed
    on the next turn. `llm_latency_ms` has no failed-turn analogue and is
    dropped with the error (unchanged from issue #115's behaviour).
  - **TUI worker usage (review secondary).** Worker `Usage` is mapped to the
    new `UiEvent::WorkerUsage` rather than reusing `UiEvent::Usage`: a worker
    reports *its own* context, so it may only move the session totals
    (`total_input` / `total_output` / `total_cache_hit` / `total_cache_miss`).
    Feeding it through `record_with_cache` blended it into the live per-turn
    cache-hit rate and let `last_prompt_tokens` momentarily show the worker's
    prompt size.
  - **TUI gate evidence (step 3, tui-mutants):** `tui-mutants.sh` → exit 0,
    8 mutants on the touched functions, **5 caught / 3 unviable / 0 missed**
    (41 min on this machine, `--jobs 10` copy mode). Note for whoever next
    touches the flow: that is well past the 20-min `tui-mutants` budget in
    `.flowcast/gates.json`, so the gate can be SIGKILLed and report a truncated
    survivor list (same failure mode as the cli-mutants note) — left untouched
    here as it is a flow-config decision, not part of this fix.
  - **Known limitation (documented, not fixed):** `cost_budget` in the
    dispatching turn cannot see worker spend, because the bridge is drained
    *after* the turn completes. The budget check therefore lags worker spend by
    one turn; a worker that blows the budget mid-turn is only caught on the
    check after the turn, never by the dispatching turn's own gate. Fixing it
    requires draining mid-turn (per step) or pre-charging the dispatch, both
    out of scope here.
  - `spawn_background_worker` mints the `TaskId` before building the runtime
    (the forwarded events carry it) but registers the `TaskState` only after a
    successful build, so a failed build cannot leave a registered task behind.
  - No new dependencies.
