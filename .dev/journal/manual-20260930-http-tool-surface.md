# Manual change journal — 2026-09-30 — http tool surface (#70 / #65)

## Date
2026-09-30

## Goal
Fix the shared root cause of #70 (`recursive http` never registers MCP tools —
AG-UI sessions see no `mcp__*` business tools) and #65 (`RECURSIVE_ALLOW_TOOLS`
silently ignored on the HTTP channel): every cross-cutting tool-surface step
lived in per-channel tails, and `Cmd::Http` had none. Consolidate the tail into
one `finish_tool_surface` and route the HTTP entry + `build_runtime` through it.

## Files touched
- `crates/recursive-cli/src/cli/builder.rs` — new `finish_tool_surface`
  (elicitation slot + `register_mcp_tools` + touched-files collector +
  `coordinator::filter_registry` in one order-preserving tail) and
  `apply_operator_allow_list` (the `retain_tools(config.allow_tools)` step,
  deliberately a SEPARATE helper because it must run as the LAST assembly
  step); `build_runtime` now calls both — finish tail, then
  `register_subagent_if_enabled`, then the allow-list (run/resume/repl/weixin
  thereby also gain the #65 allow-list, which they never applied, and no
  longer leak the sub-agent tools past it).
- `crates/recursive-cli/src/main.rs` — `Cmd::Http` runs `finish_tool_surface`
  after `build_tools` (elicitation `None`: headless, no host to answer —
  `UrlElicitationRequired` surfaces as a tool error, no hang) and
  `apply_operator_allow_list` after sub-agent registration; `run_loop` drops
  its early inline retain in favor of one `apply_operator_allow_list` call
  after its own sub-agent registration (the early retain ran before the
  sub-agent tools registered, so `agent`/`send_message`/`list_workers`
  escaped the list).
- `src/http/mod.rs` — `AppState::session_tool_registry` re-applies coordinator
  pruning + allow-list after the per-session rebind so the container tier
  (fresh registry per session) cannot leak the full toolset; source-level
  invariant pinning `finish_tool_surface` AND `apply_operator_allow_list` in
  the `Cmd::Http` block (now sliced at the Run arm so the assertion stays
  scoped); unit test for the session choke point.
- `src/runtime/builder.rs` — `AgentRuntimeBuilder::build` skips the
  unconditional `TodoWriteTool` re-register only when the registry was
  explicitly filtered (see the `surface_filtered` marker) — an operator
  allow-list without TodoWrite stays strict through build, while the default
  `AgentRuntime::builder()` path (EMPTY local registry, never filtered) keeps
  the legacy always-registered behavior pinned by the P0-2 tests.
- `src/tools/registry.rs` — `retain_tools` now marks the registry
  (`surface_filtered`, crate-private getter); the flag clones/forks with the
  registry so filtered surface contracts survive session forks.
- `src/runtime.rs` — `set_event_sink` re-points sink-dependent tools
  (TodoWrite / exit_plan_mode) only when present: HTTP/CLI sessions call it
  right after build, so an unconditional re-register would silently undo the
  allow-list on the first session.
- `src/runtime/tests.rs` — two tests pinning the sink-swap strictness
  (filtered TodoWrite / filtered exit_plan_mode survive `set_event_sink`),
  alongside the pre-existing P0-2 re-registration contract tests.

## Tests added
- `finish_tool_surface_registers_mcp_then_applies_allow_list` (cli builder;
  mock MCP server fixture — registration before filtering, allow-listed
  `mcp__mock__alpha` survives, `beta`/`Bash` dropped, collector attached)
- `finish_tool_surface_keeps_full_registry_without_filters`
- `http_entry_applies_the_shared_tool_surface_tail` (source invariant)
- `session_tool_registry_applies_allow_tools`
- `build_does_not_reinject_a_filtered_todo_write` (default EMPTY registry
  keeps TodoWrite; unfiltered explicit registry keeps it; filtered drops it)
- `retain_tools_marks_surface_filtered`
- `set_event_sink_respects_a_filtered_todo_write`
- `set_event_sink_respects_a_filtered_exit_plan_mode`

## Notes
- Live-binary verification (mock MCP server + `recursive http` + exact-set
  assertion on `/tools`) caught a second #65 leak after the initial fix:
  sub-agent tools (`agent` / `send_message` / `list_workers`) register AFTER
  every channel's pruning step, so they escaped the allow-list on ALL
  agent-loop channels, not just HTTP. That forced the restructure:
  `apply_operator_allow_list` is a separate last-step helper called after
  `register_subagent_if_enabled` on every channel.
- A flowcast pipeline-70 run (pipeline-70-0930184358) attempted the MCP-only
  inline variant and died at gates with "fix-loop exhausted" (fmt) — its
  preserved worktree is the base of this change; the inline tail is replaced
  by the shared helper and #65's scope is included.
- Known remaining gap (documented, not silently dropped): container-tier
  per-session registries are rebuilt by `ContainerToolSetProvider` and do not
  re-register MCP tools; MCP inheritance for the container tier would need the
  provider to learn about MCP servers (follow-up).
- `run_loop` keeps its own tail (elicitation/MCP/ScheduleWakeup/retain) — it
  already honored #65 and its wakeup-registration ordering is
  regression-sensitive; unifying it is follow-up cleanup, not needed for
  either issue.
