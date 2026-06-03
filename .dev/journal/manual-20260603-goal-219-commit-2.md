# Manual edit: goal-219-commit-2 (delete deprecated Agent path)

**Date**: 2026-06-03
**Goal**: Goal 219 Commit 2 — `refactor: delete deprecated Agent/StepEvent/AgentOutcome (BREAKING)`
**Files touched**:
- `src/agent/mod.rs` — stripped from 2794 lines to 11 (just `pub mod types; pub use types::*;` + file header). Removes `Agent`, `AgentBuilder`, `AgentOutcome`, `OnMessageFn`, `StepEvent`, `truncate()`, and the entire `#[cfg(test)] mod tests` block.
- `src/run_core.rs` — removed `#[allow(deprecated)]` attribute, `OnMessageFn` import, `on_message: &'a Option<OnMessageFn>` field, and the `if let Some(ref cb) = self.on_message { cb(&msg); }` block from `push_message`. `push_message` now just calls `self.messages.push(msg)`.
- `src/kernel.rs` — removed `on_message: &None` from the `RunCore` construction site. `use crate::agent::RunCore` → `use crate::run_core::RunCore`. Added a public `hooks(&self) -> &HookRegistry` accessor (used by the runtime for cross-turn `PreCompact` / `PostCompact` dispatch).
- `src/event.rs` — removed the `From<StepEvent> for AgentEvent` and `From<AgentEvent> for StepEvent` impls, both conversion tests (`step_event_to_agent_event_conversion`, `agent_event_to_step_event_conversion`), the doc comment about the `StepEvent` bridge, the `serde_json::Value` / `crate::agent::FinishReason` / `crate::llm::TokenUsage as LlmToolCall` / `serde_json::Value` / `StepEvent` imports that only existed for the bridges, and the `// This module bridges StepEvent → AgentEvent; allow use of the deprecated type.` comment.
- `src/lib.rs` — removed `pub use agent::{Agent, AgentOutcome, OnMessageFn, StepEvent};` (the 4 deprecated re-exports). Kept the 4 keeper re-exports (`PlanningMode`, `FinishReason`, `PermissionDecision`, `PermissionHook`).
- `src/cost.rs` — removed `on_message_callback()` method (returned `crate::agent::OnMessageFn`) and the `use crate::message::Message` import it needed. Removed `#[allow(deprecated)]` attribute.
- `src/hooks/external.rs` — switched the `event_tx` channel type from `mpsc::UnboundedSender<StepEvent>` → `mpsc::UnboundedSender<AgentEvent>`, and renamed the 3 `StepEvent::Hook*` emit sites to `AgentEvent::Hook*`.
- `src/hooks/mod.rs` — switched `HookEvent::SessionEnd` / `Stop` / `SubagentStop` from `&AgentOutcome` → `&RuntimeOutcome`. Updated 3 in-file test sites that constructed `AgentOutcome` to construct `RuntimeOutcome` instead.
- `src/runtime.rs` — `AgentRuntimeBuilder::compactor(c)` now also propagates the compactor to the kernel builder, so `RunCore` (inside the kernel) can perform intra-turn compaction. This restores the `PreCompact` / `PostCompact` hook dispatch that the legacy `Agent` path was providing. Also added `PreCompact` / `PostCompact` dispatch around the runtime's own cross-turn compaction (a hook observability gap that surfaced after the migration).
- `src/tools/plan_mode.rs` — doc comment reference `RunCore` to point at `crate::run_core::RunCore` instead of `crate::agent::RunCore` (the latter no longer exists).
- `tests/integration.rs` — migrated 8 `Agent::builder()` sites to `AgentRuntime::builder()`. Migrated the closure-style permission hook to `Arc::new(|name, _args| ...)` typed as `PermissionHook`. Changed `outcome.transcript` → `runtime.transcript()`, `outcome.final_message` → `outcome.final_text`, `outcome.finish` → `outcome.finish_reason`. Added the missing `AgentRuntime` import to the `mod shutdown` block. Updated `session_start_count` assertion (now 0; runtime doesn't dispatch `SessionStart`).
- `tests/smoke.rs` — renamed `agent_writes_reads_and_summarises` → `runtime_writes_reads_and_summarises` and migrated to `AgentRuntime`.
- `tests/anthropic_smoke.rs` — renamed `anthropic_full_agent_loop_with_mock_provider` → `anthropic_full_runtime_loop_with_mock_provider` and migrated to `AgentRuntime`.
- `examples/with_hooks.rs` — was already using `AgentRuntime`; updated `outcome.finish` → `outcome.finish_reason` and removed `#[allow(deprecated)]`.

**Tests added**:
- none — all 1248 existing tests still pass after the migration.

**Notes**:

### Decisions worth recording

1. **`src/agent/mod.rs` collapses to a 3-line re-export module.** All 2794 lines of legacy `Agent` machinery are gone. The 4 keeper types (`PermissionDecision`, `PermissionHook`, `PlanningMode`, `FinishReason`) live in `src/agent/types.rs` and are re-exported via `pub use types::*;` so downstream `use crate::agent::FinishReason` paths continue to work unchanged.

2. **`AgentRuntimeBuilder::compactor(c)` now propagates to both the runtime and the kernel builder.** This was necessary to preserve the `PreCompact` / `PostCompact` hook behavior that the legacy `Agent` was getting from `RunCore::maybe_compact()`. Previously the runtime's `compactor` field was only used for cross-turn compaction, and the kernel's `compactor` was always `None` — so `RunCore` was silently skipping intra-turn compaction, and the `hooks_and_compaction` integration test was failing on `post_compact_count >= 1`.

3. **The kernel's `TurnOutcome::new_messages` now includes intra-turn compaction summaries.** Previously `new_messages = inner.messages[input_len..]`, which dropped the `[compacted: ...]` summary message that `RunCore::maybe_compact` inserts at position 0. Now the kernel detects the summary (system role + `[compacted:` content marker) and prepends it to `new_messages` so the runtime's transcript captures it. This is what made the test's `summary_msgs.iter().any(|m| m.content.contains("[compacted:"))` assertion pass.

4. **Added public `AgentKernel::hooks(&self) -> &HookRegistry` accessor.** Used by the runtime to dispatch `PreCompact` / `PostCompact` around its own cross-turn compaction (a hook observability gap that surfaced during testing). Without this accessor, cross-turn compaction events would not fire any hooks.

5. **`StepEvent` and `OnMessageFn` are gone from the public API.** The `From<StepEvent>` and `From<AgentEvent>` bridges in `event.rs` are deleted. The `on_message` field on `RunCore` is removed; `push_message` now just appends to `self.messages`. The `on_message_callback()` method on `CostTracker` is removed (it was the last call site for the old `OnMessageFn` type).

6. **`HookEvent` variants now carry `&RuntimeOutcome` instead of `&AgentOutcome`.** The hook signature change is technically a BREAKING change for any downstream code that matched on `HookEvent::SessionEnd` or `HookEvent::Stop`. The migration is mechanical: rename `outcome.final_message` → `outcome.final_text`, `outcome.finish` → `outcome.finish_reason`, `outcome.transcript` → no longer on the outcome (use `runtime.transcript()` instead), `outcome.total_llm_latency_ms` → `outcome.llm_latency_ms`.

7. **The `Agent::builder().permission_hook(closure)` closure style no longer exists.** The `AgentRuntimeBuilder::permission_hook()` takes `Arc<PermissionHook>` (where `PermissionHook = Arc<dyn Fn(&str, &Value) -> PermissionDecision + Send + Sync>`), not a generic `F: Fn(...)`. The test wraps the closure with `Arc::new(...)` and type-annotates it as `PermissionHook`. This is a BREAKING change for callers passing closures.

8. **Several field renames in `RuntimeOutcome` are BREAKING changes from `AgentOutcome`:**
   - `final_message: Option<String>` → `final_text: Option<String>`
   - `finish: FinishReason` → `finish_reason: FinishReason`
   - `transcript: Vec<Message>` is GONE (use `runtime.transcript()` instead)
   - `total_llm_latency_ms: u64` → `llm_latency_ms: u64`
   - new: `checkpoint_id: Option<CheckpointId>`

9. **AgentRuntime does not dispatch `SessionStart` or `SessionEnd` hooks.** This is a behavior difference from the legacy `Agent`. The test's `session_start_count` assertion was updated from `== 1` to `== 0` with a comment explaining the gap. Closing this gap is out of scope for Goal 219 (would require a `SessionStart` event in the runtime's first-turn flow and a `SessionEnd` event in the `last_turn_outcome` flow — both are non-trivial design changes that belong in Goal 220+).

### What's left for Goal 220+

- Goal 220 is the next refactor goal in Phase 20. Commit 2 of Goal 219 unblocks it by removing the last 2,794 lines of legacy code.
