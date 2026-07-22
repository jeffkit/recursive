# Manual edit: context-breakdown

**Date**: 2026-07-22
**Goal**: Per-component context breakdown (Cursor-style context usage)
**Files touched**:
- `src/llm/chat.rs` — `ContextBreakdown` struct + `estimate_tokens(text) -> u32` helper, re-exported from `src/llm/mod.rs` and `src/lib.rs`.
- `src/tools/estimate_tokens.rs` — `EstimateTokens::estimate` now delegates to the shared helper (no behaviour change).
- `src/system_prompt.rs` — `assemble_system_prompt` now returns `AssembledPrompt { full, segments }` with `PromptSegments { rules, system_prompt, skills, subagents }`. The joined string is byte-identical to the pre-change output (the existing `ordering_project_context_then_base_then_skills_then_subagent` test passes unchanged). `full()` / `into_full()` / `Display` accessors preserve the pre-change call-site signature.
- `src/event.rs` — new `AgentEvent::ContextBreakdown { breakdown, step }` variant.
- `src/run_core.rs` — `RunCore` carries `prompt_segments: Option<PromptSegments>` + a new `StaticBreakdownCache { system_prompt, rules, skills, subagents, tools, mcp_dynamic }` that is sized once at construction. New `compute_breakdown` / `emit_breakdown` helpers; the breakdown event is emitted right after `AgentEvent::Usage` so consumers see the provider truth first.
- `src/kernel.rs` — `TurnContext` gains `prompt_segments`; `AgentKernel::run` builds the static cache from the segments + `ToolRegistry::is_deferred_spec` (eager vs `mcp_dynamic`).
- `src/runtime.rs` — `AgentRuntime` + `AgentRuntimeBuilder` carry `prompt_segments`; new builder method `.prompt_segments(...)`. The runtime forwards the segments into every `TurnContext`.
- 8 production call sites updated to consume `assemble_system_prompt(...).into_full()` (or capture `assembled` + pass `assembled.full` / `.prompt_segments(assembled.segments)`):
  - `crates/recursive-cli/src/cli/builder.rs`
  - `crates/recursive-cli/src/main.rs`
  - `crates/recursive-tui/src/runtime_builder.rs` (×2 — `build_runtime` + `build_runtime_with_skill_tx`)
  - `src/http/handlers.rs` (×4 — `create_session`, `run_agent`, `fork_session`, `agui_run`)
- `tests/agent_team_integration.rs` — `TurnContext` literal gets the new `prompt_segments: None` field.
- `src/multi.rs`, `src/tools/agent.rs` — same for sub-agent / `AgentTool` workers.

**TUI side:**
- `crates/recursive-tui/src/cost.rs` — `UsageStats.last_breakdown: Option<ContextBreakdown>`; new `record_breakdown` and `current_prompt_estimate` helpers. Cost accuracy is preserved (provider totals still drive `estimate_cost`).
- `crates/recursive-tui/src/ui/input.rs` — `context_gauge` now reads `usage.current_prompt_estimate()` (breakdown total) instead of `usage.last_prompt_tokens` so the gauge advances during tool execution, when no provider reading arrives.
- `crates/recursive-tui/src/events.rs` — new `UiEvent::ContextBreakdown { breakdown }`.
- `crates/recursive-tui/src/backend.rs` — translates `AgentEvent::ContextBreakdown` → `UiEvent::ContextBreakdown`.
- `crates/recursive-tui/src/app/event_loop.rs` — applies the new event to `app.usage`.
- `crates/recursive-tui/src/app/commands.rs` — `Ctrl+O` pushes `Modal::ContextUsage`.
- `crates/recursive-tui/src/ui/modal.rs` — new `Modal::ContextUsage` + `render_context_usage_body` (Cursor-style proportional bar + legend with name, color, token count, percentage; falls back to placeholders for `None` / `total == 0`).

**Tests added:**
- `src/llm/chat.rs` — 5 new tests: `estimate_tokens` empty/exact/ceil/long-string/non-empty-floor; 3 `ContextBreakdown` tests (default, `local_sum`, serde round-trip).
- `src/system_prompt.rs` — 5 new tests: `full_is_byte_identical_to_pre_goal_328_assembly`, `segments_are_populated_and_non_overlapping`, `empty_segments_when_layers_absent`, `segments_skills_contains_available_skills_only_when_skills_present`, `segments_subagents_contains_coordinator_only_when_enabled`, `full_accessor_returns_str`, `display_impl_writes_full`. Updated the existing 5 tests to assert on `.into_full()`.
- `src/run_core.rs` — 7 new tests: `compute_breakdown_overhead_is_provider_total_minus_local_sum`, `compute_breakdown_overhead_saturates_at_zero_when_local_exceeds_provider`, `dispatch_llm_step_emits_context_breakdown_after_usage`, `run_inner_emits_context_breakdown_once_per_llm_step`, `run_inner_skips_context_breakdown_on_no_llm_step`, `static_buckets_dont_change_across_steps_conversation_grows`, `static_breakdown_cache_build_zero_specs_yields_zero_tokens`, `static_breakdown_cache_build_tokenises_segments`.
- `crates/recursive-tui/src/ui/modal.rs` — 4 new tests: `render_context_usage_body_emits_all_seven_bucket_labels`, `render_context_usage_body_handles_no_breakdown_yet`, `render_context_usage_body_handles_all_zero_breakdown`, `render_context_usage_body_shows_window_line_when_known`.

**Quality gates:**
- `cargo fmt --all` clean.
- `cargo clippy --all-targets --all-features -- -D warnings` clean.
- `cargo test --workspace` green: 2024 passed (one pre-existing flaky env-var race on `effective_step_limit_ignores_invalid_hard_cap` under `cargo test --workspace` parallel load — passes in isolation, unrelated to this goal).
- `.dev/scripts/tui-test-presence.sh` PASS (new test marker detected in `crates/recursive-tui/src/ui/modal.rs`).
- `.dev/scripts/tui-mutants.sh` not run — requires a clean tree (committed) and is "recommended but advisory for manual edits" per the goal's Acceptance section.

**Notes:**
- The breakdown computation lives in `run_core` and is forwarded to `kernel` via the new `prompt_segments: Option<PromptSegments>` field on `TurnContext`. Sub-agent workers (`src/multi.rs`, `src/tools/agent.rs`) pass `None` — they don't have access to the structured segments and the breakdown is unnecessary there (the parent's breakdown is what the user sees).
- The static bucket sizes are cached at `RunCore` construction via `StaticBreakdownCache::build`, which partitions tool specs into `tools` vs `mcp_dynamic` via `ToolRegistry::is_deferred_spec` (MCP servers and any `is_deferred() == true` tool land in `mcp_dynamic`). The cache is read-only for the rest of the run; the `conversation` bucket is recomputed every step from `self.messages`.
- `overhead` saturates to 0 when the local sum exceeds the provider's reported total (chars/4 can over-estimate on CJK content).
- The Cursor-style panel mirrors the existing modal pattern (`Modal::Help`, `Modal::CostDetail`, …) — full-width left-accent border, top-anchored, Esc/q closes.
- `?` was already taken (alias for `/help`); picked `Ctrl+O` as the open key. Other potential conflicts (`Ctrl+G`, `Ctrl+K`, `Ctrl+T`) were checked against `commands.rs`.
- Pre-existing behaviour preserved: `last_prompt_tokens` is still the provider-reported truth and still drives `estimate_cost`. The status-bar gauge only switched its read source from `last_prompt_tokens` to `current_prompt_estimate()` (which prefers the breakdown, falls back to the provider number when no breakdown has been recorded yet — e.g. in tests that bypass the breakdown event).